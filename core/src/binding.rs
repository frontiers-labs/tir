//! What a `binds:` declaration means at run time: the ranges an op's
//! [`Binding`] names, the checks that hold them aligned, and the one syntax
//! every declared theta and gamma shares.
//!
//! A theta prints as `%r = dialect.op (%port = %init, ..) { .. }` and a
//! gamma as `%r = dialect.op %pred args(%in, ..) (%port, ..) { .. } (..) { .. }`.
//! Types are not spelled: a port has its init's type, a theta result its
//! init's, and a gamma result the type of the first arm's result. A memory
//! state is carried like any other value.

use std::ops::Range;

use crate::attributes::{AttributeValue, Predicate};
use crate::builtin::{AddIOp, CmpIOp, IntegerType};
use crate::parse::Span;
use crate::parse::common::Cursor;
use crate::parse::text::Parser;
use crate::{
    Binding, Context, Error, IRFormatter, OpHandle, RegionId, TypeId, ValueId, region_format,
};

/// The value-operand range of each declared operand group, read off the
/// segment sizes a variadic op records; a fixed-arity op has one operand per
/// group.
pub fn operand_segments(op: &OpHandle, groups: usize) -> Vec<Range<usize>> {
    let sizes: Vec<usize> = match op.attr("operand_segment_sizes") {
        Some(AttributeValue::Array(items)) => items
            .iter()
            .map(|item| match item {
                AttributeValue::UInt(size) => *size as usize,
                _ => 0,
            })
            .collect(),
        _ => vec![1; groups],
    };
    let mut start = 0;
    sizes
        .iter()
        .map(|&size| {
            let range = start..start + size;
            start += size;
            range
        })
        .collect()
}

/// How many ports (or results) the op's `index`-th region has; zero for a
/// region the op does not hold, so a gamma with no arms still answers.
pub fn region_list_len(context: &Context, op: &OpHandle, index: usize, ports: bool) -> usize {
    let Some(&region) = op.regions().get(index) else {
        return 0;
    };
    let region = context.get_region(region);
    if ports {
        region.ports().len()
    } else {
        region.results().len()
    }
}

fn fail(message: String) -> Error {
    Error::VerificationError(message)
}

/// How many entries of a list `range` reaches: the range's length when it
/// fits, else what is left after its start.
fn reach(range: &Range<usize>, len: usize) -> usize {
    if range.end <= len {
        range.len()
    } else {
        len.saturating_sub(range.start)
    }
}

fn check_types(
    context: &Context,
    name: &str,
    expected: &[ValueId],
    found: &[ValueId],
    what: &str,
    against: &str,
) -> Result<(), Error> {
    for (index, (&expected, &found)) in expected.iter().zip(found).enumerate() {
        if context.get_value(expected).ty() != context.get_value(found).ty() {
            return Err(fail(format!(
                "{name} {what} {index} must have the type of {against} {index}"
            )));
        }
    }
    Ok(())
}

fn slice(values: &[ValueId], range: &Range<usize>) -> Vec<ValueId> {
    values[range.start.min(values.len())..range.end.min(values.len())].to_vec()
}

/// Whether `found` and `declared` name the same value. A bound or a step is a
/// value, not a spelling: the simplifier merges congruent constants, so a body
/// that computes the counter's advance over its own `1` states the recurrence
/// the op declares over an outer `1` just as well.
fn names_same(context: &Context, found: ValueId, declared: ValueId) -> bool {
    if found == declared {
        return true;
    }
    let constant = |value: ValueId| {
        context
            .get_value(value)
            .defining_op()
            .filter(|&op| context.has_operation(op))
            .and_then(|op| context.get_op(op).as_interface::<dyn crate::ConstantLike>())
            .map(|op| op.constant_value())
    };
    match (constant(found), constant(declared)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// The binding a loop or a gate declares: what it carries in, through and out.
pub fn declared(op: &OpHandle) -> Option<Binding> {
    if let Some(theta) = op.clone().as_interface::<dyn crate::Theta>() {
        return Some(theta.carried());
    }
    op.clone()
        .as_interface::<dyn crate::Gamma>()
        .map(|gamma| gamma.forwarded())
}

/// Where one state a loop or a gate carries sits, as absolute positions into
/// the op's operands and results and its regions' ports and results. A
/// loop carries a state through one aligned list; a gate forwards its
/// states into every arm and joins them back by a second alignment, and its
/// i-th forwarded and i-th joined state are one chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateSlot {
    pub operand: usize,
    pub port: usize,
    /// A loop's continue result; a gate has none.
    pub continue_: Option<usize>,
    pub exit: usize,
    pub result: usize,
}

/// The states a loop or a gate carries, in chain order.
pub fn state_slots(context: &Context, op: &OpHandle) -> Vec<StateSlot> {
    let Some(binding) = declared(op) else {
        return Vec::new();
    };
    let Some(&region) = op.regions().first() else {
        return Vec::new();
    };
    let ports = context.get_region(region).ports();
    let forwarded: Vec<usize> = (0..binding.ports.len())
        .filter(|&index| ports[binding.ports.start + index].is_state())
        .collect();
    if op.has_interface::<dyn crate::Theta>() {
        return forwarded
            .into_iter()
            .map(|index| StateSlot {
                operand: binding.operands.start + index,
                port: binding.ports.start + index,
                continue_: Some(binding.continue_.start + index),
                exit: binding.exit.start + index,
                result: binding.results.start + index,
            })
            .collect();
    }
    let results = op.results();
    let joined = (0..binding.results.len()).filter(|&index| {
        context
            .get_value(results[binding.results.start + index])
            .is_state()
    });
    forwarded
        .into_iter()
        .zip(joined)
        .map(|(forwarded, joined)| StateSlot {
            operand: binding.operands.start + forwarded,
            port: binding.ports.start + forwarded,
            continue_: None,
            exit: binding.exit.start + joined,
            result: binding.results.start + joined,
        })
        .collect()
}

/// Whether every region of `op` hands the state at `slot` straight back, in
/// every result group: the chain flows past the operation rather than
/// through it, since nothing under it changed the memory.
pub fn forwards_state(context: &Context, op: &OpHandle, slot: StateSlot) -> bool {
    op.regions().iter().all(|&region| {
        let region = context.get_region(region);
        let port = region.ports()[slot.port].id();
        let results = region.results();
        slot.continue_
            .into_iter()
            .chain([slot.exit])
            .all(|index| results.get(index) == Some(&port))
    })
}

/// Checks a theta's declared alignment: five ranges of one length, one type per
/// offset, and a boolean predicate.
pub fn verify_theta(
    context: &Context,
    op: &OpHandle,
    name: &str,
    body: RegionId,
    binding: &Binding,
    predicate: usize,
) -> Result<(), Error> {
    let region = context.get_region(body);
    let (ports, results) = (region.ports(), region.results());
    let ports: Vec<ValueId> = ports.iter().map(crate::Value::id).collect();
    let (operands, op_results) = (op.operands(), op.results());
    let n = binding.operands.len();

    let Some(&decides) = results.get(predicate) else {
        return Err(fail(format!("{name} body must produce a predicate")));
    };
    if context.get_value(decides).ty() != IntegerType::new(context, 1) {
        return Err(fail(format!("{name} predicate must have type i1")));
    }
    let counts = [
        (reach(&binding.ports, ports.len()), "ports"),
        (reach(&binding.continue_, results.len()), "continue values"),
        (reach(&binding.exit, results.len()), "exit values"),
        (reach(&binding.results, op_results.len()), "results"),
    ];
    for (found, what) in counts {
        if found != n {
            return Err(fail(format!(
                "{name} carries {n} values but has {found} {what}"
            )));
        }
    }
    let inits = slice(&operands, &binding.operands);
    let carried = slice(&ports, &binding.ports);
    check_types(context, name, &inits, &carried, "port", "init")?;
    check_types(
        context,
        name,
        &carried,
        &slice(&results, &binding.continue_),
        "continue value",
        "port",
    )?;
    check_types(
        context,
        name,
        &carried,
        &slice(&results, &binding.exit),
        "exit value",
        "port",
    )?;
    check_types(
        context,
        name,
        &carried,
        &slice(&op_results, &binding.results),
        "result",
        "port",
    )
}

/// Checks a gamma's declared alignment on every arm: ports typed like the
/// forwarded operands, results typed like the op's. An arm's port and result
/// lists are aligned whole, so each arm is read in full rather than through
/// the ranges arm 0 gave the binding.
pub fn verify_gamma(
    context: &Context,
    op: &OpHandle,
    name: &str,
    arms: &[RegionId],
    binding: &Binding,
) -> Result<(), Error> {
    if arms.is_empty() {
        return Err(fail(format!("{name} needs at least one arm")));
    }
    let inputs = slice(&op.operands(), &binding.operands);
    let results = slice(&op.results(), &binding.results);
    for (index, &arm) in arms.iter().enumerate() {
        let region = context.get_region(arm);
        let ports: Vec<ValueId> = region.ports().iter().map(crate::Value::id).collect();
        let produced = region.results();
        if ports.len() != inputs.len() {
            return Err(fail(format!(
                "{name} arm {index} takes {} values but the op forwards {}",
                ports.len(),
                inputs.len()
            )));
        }
        if produced.len() != results.len() {
            return Err(fail(format!(
                "{name} arm {index} produces {} values but the op produces {}",
                produced.len(),
                results.len()
            )));
        }
        let arm_name = format!("{name} arm {index}");
        check_types(context, &arm_name, &inputs, &ports, "port", "input")?;
        check_types(context, &arm_name, &results, &produced, "value", "result")?;
    }
    let (forwarded, joined) = (
        context.states_among(&inputs).len(),
        context.states_among(&results).len(),
    );
    if forwarded != joined {
        return Err(fail(format!(
            "{name} forwards {forwarded} states but joins {joined}: each chain enters and leaves once"
        )));
    }
    Ok(())
}

/// Checks the shape `counted:` pins onto a theta: the predicate is
/// `cmpi slt(counter, ub)`, the counter continues as `addi(counter, step)`, and
/// every port leaves the loop unchanged.
#[allow(clippy::too_many_arguments)]
pub fn verify_counted(
    context: &Context,
    name: &str,
    body: RegionId,
    binding: &Binding,
    predicate: usize,
    induction: usize,
    upper_bound: ValueId,
    step: ValueId,
) -> Result<(), Error> {
    let region = context.get_region(body);
    let ports: Vec<ValueId> = region.ports().iter().map(crate::Value::id).collect();
    let results = region.results();
    if induction >= binding.ports.len() {
        return Err(fail(format!(
            "{name} carries no port {induction} for its counter"
        )));
    }
    let counter = ports[binding.ports.start + induction];

    let predicate = context.get_value(results[predicate]).defining_op();
    let compares = predicate.is_some_and(|op| {
        let op = context.get_op(op);
        op.is::<CmpIOp>()
            && op.attr("predicate") == Some(AttributeValue::Predicate(Predicate::Slt))
            && matches!(op.operands().as_slice(),
                [lhs, rhs] if *lhs == counter && names_same(context, *rhs, upper_bound))
    });
    if !compares {
        return Err(fail(format!(
            "{name} predicate must be cmpi slt of the counter and the upper bound"
        )));
    }
    let next = results[binding.continue_.start + induction];
    let advances = context.get_value(next).defining_op().is_some_and(|op| {
        let op = context.get_op(op);
        op.is::<AddIOp>()
            && matches!(op.operands().as_slice(),
                [lhs, rhs] if (*lhs == counter && names_same(context, *rhs, step))
                    || (*rhs == counter && names_same(context, *lhs, step)))
    });
    if !advances {
        return Err(fail(format!("{name} must advance the counter by the step")));
    }
    for (index, (&exit, &port)) in slice(&results, &binding.exit)
        .iter()
        .zip(&slice(&ports, &binding.ports))
        .enumerate()
    {
        if exit != port {
            return Err(fail(format!(
                "{name} exit value {index} must be port {index}"
            )));
        }
    }
    Ok(())
}

fn print_pairs(
    fmt: &mut IRFormatter,
    ports: &[ValueId],
    inits: &[ValueId],
) -> Result<(), std::fmt::Error> {
    for (index, (port, init)) in ports.iter().zip(inits).enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{} = %{}", port.number(), init.number()))?;
    }
    Ok(())
}

/// Print `(%port = %init, ..)`, or nothing when the op carries nothing.
pub fn print_port_bindings(
    fmt: &mut IRFormatter,
    ports: &[ValueId],
    inits: &[ValueId],
) -> Result<(), std::fmt::Error> {
    if ports.is_empty() {
        return Ok(());
    }
    fmt.write(" (")?;
    print_pairs(fmt, ports, inits)?;
    fmt.write(")")
}

fn port_ids(context: &Context, region: RegionId) -> Vec<ValueId> {
    context
        .get_region(region)
        .ports()
        .iter()
        .map(crate::Value::id)
        .collect()
}

/// The generic theta printer: the ports bound to their inits, then the body.
pub fn print_theta(
    fmt: &mut IRFormatter,
    op: &OpHandle,
    name: &str,
    body: RegionId,
    binding: &Binding,
) -> Result<(), std::fmt::Error> {
    let context = op.context.upgrade();
    region_format::print_result_prefix(fmt, op)?;
    fmt.write(name)?;
    print_port_bindings(
        fmt,
        &slice(&port_ids(&context, body), &binding.ports),
        &slice(&op.operands(), &binding.operands),
    )?;
    region_format::print_region(fmt, &context, &context.get_region(body))
}

/// The generic gamma printer: the predicate, the forwarded operands, then
/// each arm's ports and body.
pub fn print_gamma(
    fmt: &mut IRFormatter,
    op: &OpHandle,
    name: &str,
    predicate: ValueId,
    arms: &[RegionId],
    binding: &Binding,
) -> Result<(), std::fmt::Error> {
    let context = op.context.upgrade();
    region_format::print_result_prefix(fmt, op)?;
    fmt.write(format!("{name} %{}", predicate.number()))?;
    let inputs = slice(&op.operands(), &binding.operands);
    if !inputs.is_empty() {
        fmt.write(" args(")?;
        region_format::print_value_list(fmt, &inputs)?;
        fmt.write(")")?;
    }
    for &arm in arms {
        let ports = slice(&port_ids(&context, arm), &binding.ports);
        if !ports.is_empty() {
            fmt.write(if fmt.at_line_start() { "(" } else { " (" })?;
            region_format::print_value_list(fmt, &ports)?;
            fmt.write(")")?;
        }
        region_format::print_region(fmt, &context, &context.get_region(arm))?;
    }
    Ok(())
}

/// What the generic theta syntax names: the inits, in the order the ports
/// were bound, and the body those ports belong to.
pub struct ParsedTheta {
    pub inits: Vec<ValueId>,
    pub body: RegionId,
    pub result_types: Vec<TypeId>,
}

/// What the generic gamma syntax names.
pub struct ParsedGamma {
    pub predicate: ValueId,
    pub inputs: Vec<ValueId>,
    pub arms: Vec<RegionId>,
    pub result_types: Vec<TypeId>,
}

type ParseResult<T> = Result<T, (Span, Error)>;

pub(crate) fn expect(parser: &mut Parser, token: &'static str) -> ParseResult<()> {
    if parser.parse_token(token) {
        Ok(())
    } else {
        Err((parser.span(), Error::ExpectedToken(token)))
    }
}

pub(crate) fn value(parser: &mut Parser, context: &Context) -> ParseResult<ValueId> {
    let name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    Ok(parser.resolve_value(context, name))
}

/// Mint a port named `name` with the type `init` has and bind the name to it.
fn bind_port(parser: &mut Parser, context: &Context, name: &str, ty: TypeId) -> crate::Value {
    let port = context.create_value(ty, None);
    parser.define_value(name, port.id());
    port
}

/// The ports a `(%port = %init, ..)` clause binds, each minted with its
/// init's type, and the inits they are bound to.
#[derive(Default)]
pub struct PortBindings {
    pub ports: Vec<crate::Value>,
    pub inits: Vec<ValueId>,
}

/// Parse an optional `(%port = %init, ..)` clause.
pub fn parse_port_bindings(parser: &mut Parser, context: &Context) -> ParseResult<PortBindings> {
    let mut bound = PortBindings::default();
    if !parser.parse_token("(") {
        return Ok(bound);
    }
    while parser.peek_char() == Some('%') {
        let name = parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
            .to_string();
        expect(parser, "=")?;
        let init = value(parser, context)?;
        let ty = context.get_value(init).ty();
        bound.ports.push(bind_port(parser, context, &name, ty));
        bound.inits.push(init);
        if !parser.parse_token(",") {
            break;
        }
    }
    expect(parser, ")")?;
    Ok(bound)
}

/// Put a theta body's results in binding order. The text lists the values
/// the body names, then its states; the binding wants the predicate, then
/// what the next iteration carries, then what the loop leaves, each of those
/// values then states as the ports are.
pub fn order_theta_results(context: &Context, body: RegionId) {
    let region = context.get_region(body);
    let (values, states) = (region.value_results(), region.state_results());
    let (value_ports, state_ports) = (
        region.value_arguments().len(),
        region.state_arguments().len(),
    );
    if values.len() != 1 + 2 * value_ports || states.len() != 2 * state_ports {
        return;
    }
    let mut results = vec![values[0]];
    results.extend(&values[1..1 + value_ports]);
    results.extend(&states[..state_ports]);
    results.extend(&values[1 + value_ports..]);
    results.extend(&states[state_ports..]);
    context.set_region_results(body, results);
}

/// Parse the generic theta syntax after its mnemonic.
pub fn parse_theta(parser: &mut Parser, context: &Context) -> ParseResult<ParsedTheta> {
    let bound = parse_port_bindings(parser, context)?;
    let result_types = bound.ports.iter().map(crate::Value::ty).collect();
    let body = parser
        .parse_region_with_entry_args(context, bound.ports)?
        .id();
    order_theta_results(context, body);
    Ok(ParsedTheta {
        inits: bound.inits,
        body,
        result_types,
    })
}

/// Parse the generic gamma syntax after its mnemonic.
pub fn parse_gamma(parser: &mut Parser, context: &Context) -> ParseResult<ParsedGamma> {
    let predicate = value(parser, context)?;
    let mut inputs = vec![];
    if parser.parse_token("args") {
        expect(parser, "(")?;
        while parser.peek_char() == Some('%') {
            inputs.push(value(parser, context)?);
            if !parser.parse_token(",") {
                break;
            }
        }
        expect(parser, ")")?;
    }
    let mut arms = vec![];
    loop {
        let mut ports = vec![];
        if parser.parse_token("(") {
            for (index, &input) in inputs.iter().enumerate() {
                if index > 0 {
                    expect(parser, ",")?;
                }
                let name = parser
                    .parse_value_ref()
                    .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
                    .to_string();
                let ty = context.get_value(input).ty();
                ports.push(bind_port(parser, context, &name, ty));
            }
            expect(parser, ")")?;
        } else if parser.peek_char() != Some('{') {
            break;
        }
        arms.push(parser.parse_region_with_entry_args(context, ports)?.id());
    }
    let Some(&first) = arms.first() else {
        return Err((parser.span(), Error::ExpectedToken("{")));
    };
    let first = context.get_region(first);
    Ok(ParsedGamma {
        predicate,
        inputs,
        arms,
        result_types: first
            .results()
            .iter()
            .map(|&result| context.get_value(result).ty())
            .collect(),
    })
}
