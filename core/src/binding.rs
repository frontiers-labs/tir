//! What a `binds:` declaration means at run time: the ranges an op's
//! [`Binding`] names, the checks that hold them aligned, and the one syntax
//! every declared theta and gamma shares.
//!
//! A theta prints as `%r = dialect.op (%port = %init, ..) { .. }` and a
//! gamma as `%r = dialect.op %pred args(%in, ..) (%port, ..) { .. } (..) { .. }`.
//! A gamma whose cases are not its arm indices labels each arm:
//! `case 10 (%port, ..) { .. } .. default (..) { .. }`.
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

/// The attribute a gamma names its cases in; see [`crate::Gamma::cases`].
pub const GAMMA_CASES: &str = "cases";

/// A gamma's cases: the ones it names, else the arm indices.
pub fn gamma_cases(op: &OpHandle, arms: usize) -> Vec<u64> {
    match op.attr(GAMMA_CASES) {
        Some(AttributeValue::Array(items)) => items
            .iter()
            .map(|item| match item {
                AttributeValue::UInt(bits) => *bits,
                _ => u64::MAX,
            })
            .collect(),
        _ => (0..arms.saturating_sub(1) as u64).collect(),
    }
}

/// The attribute naming `cases`, or `None` where they are the arm indices a
/// gamma has without one.
pub fn gamma_cases_attribute(cases: &[u64]) -> Option<AttributeValue> {
    let indexes = cases
        .iter()
        .enumerate()
        .all(|(index, &case)| case == index as u64);
    (!indexes).then(|| {
        AttributeValue::Array(
            cases
                .iter()
                .map(|&case| AttributeValue::UInt(case))
                .collect::<Vec<_>>()
                .into(),
        )
    })
}

fn predicate_width(context: &Context, predicate: ValueId) -> u32 {
    crate::sem::egraph::type_width(context, context.get_value(predicate).ty()).unwrap_or(64)
}

/// `value` truncated to `width` bits.
pub fn truncate_bits(value: i64, width: u32) -> u64 {
    if width >= 64 {
        value as u64
    } else {
        value as u64 & ((1 << width) - 1)
    }
}

/// `bits` of a `width`-bit value read as signed, the way a case is spelled.
pub fn signed_bits(bits: u64, width: u32) -> i64 {
    if width == 0 || width >= 64 {
        return bits as i64;
    }
    let shift = 64 - width;
    ((bits << shift) as i64) >> shift
}

/// The cases a gamma names, checked against its arms and predicate: one per
/// arm but the last, distinct, and within the predicate's width. A one-bit
/// predicate has nothing to name: its gamma indexes its arms.
fn verify_gamma_cases(
    context: &Context,
    op: &OpHandle,
    name: &str,
    predicate: ValueId,
    arms: usize,
) -> Result<(), Error> {
    let Some(attribute) = op.attr(GAMMA_CASES) else {
        return Ok(());
    };
    let AttributeValue::Array(items) = attribute else {
        return Err(fail(format!("{name} cases must be an array")));
    };
    if items.len() + 1 != arms {
        return Err(fail(format!(
            "{name} names {} cases for {arms} arms: every arm but the default has one",
            items.len()
        )));
    }
    let width = predicate_width(context, predicate);
    if width == 1 {
        return Err(fail(format!(
            "{name} on a one-bit predicate indexes its arms and names no cases"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for item in items.iter() {
        let &AttributeValue::UInt(bits) = item else {
            return Err(fail(format!("{name} cases must be unsigned bit patterns")));
        };
        if truncate_bits(bits as i64, width) != bits {
            return Err(fail(format!(
                "{name} case {bits:#x} does not fit its {width}-bit predicate"
            )));
        }
        if !seen.insert(bits) {
            return Err(fail(format!("{name} names case {bits:#x} twice")));
        }
    }
    Ok(())
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
        return Some(theta.binding());
    }
    if let Some(gamma) = op.clone().as_interface::<dyn crate::Gamma>() {
        return Some(gamma.binding());
    }
    op.clone()
        .as_interface::<dyn crate::RegionBinding>()
        .map(|region| region.binding())
}

/// One value a loop carries, named on every side of the op: the operand it
/// enters on, the port the body reads it through, what the next iteration
/// takes, what the loop leaves, and the op result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carried {
    pub init: ValueId,
    pub port: ValueId,
    pub next: ValueId,
    pub exit: ValueId,
    pub result: ValueId,
}

/// The values a loop carries, in port order; empty for anything but a loop.
pub fn carried(context: &Context, op: &OpHandle) -> Vec<Carried> {
    let Some(theta) = op.clone().as_interface::<dyn crate::Theta>() else {
        return Vec::new();
    };
    let binding = theta.binding();
    let region = context.get_region(theta.body());
    let (ports, results) = (region.ports(), region.results());
    let (operands, op_results) = (op.operands(), op.results());
    (0..binding.ports.len())
        .map(|index| Carried {
            init: operands[binding.operands.start + index],
            port: ports[binding.ports.start + index].id(),
            next: results[binding.continue_.start + index],
            exit: results[binding.exit.start + index],
            result: op_results[binding.results.start + index],
        })
        .collect()
}

/// One state a loop or a gate carries: the state it enters on, the port each
/// region reads it through, the state each region leaves (and, for a loop,
/// the one the next iteration takes), and the state the op leaves. A gate's
/// i-th forwarded and i-th joined state are one chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateChain {
    pub entered: ValueId,
    pub ports: Vec<ValueId>,
    pub next: Option<ValueId>,
    pub exits: Vec<ValueId>,
    pub left: ValueId,
}

/// The states a loop or a gate carries, in chain order.
pub fn state_chains(context: &Context, op: &OpHandle) -> Vec<StateChain> {
    if op.has_interface::<dyn crate::Theta>() {
        return carried(context, op)
            .into_iter()
            .filter(|value| context.is_state_type(context.get_value(value.port).ty()))
            .map(|value| StateChain {
                entered: value.init,
                ports: vec![value.port],
                next: Some(value.next),
                exits: vec![value.exit],
                left: value.result,
            })
            .collect();
    }
    let Some(gamma) = op.clone().as_interface::<dyn crate::Gamma>() else {
        let Some(owner) = op.clone().as_interface::<dyn crate::RegionBinding>() else {
            return Vec::new();
        };
        let binding = owner.binding();
        let region = context.get_region(owner.region());
        let forwarded = (0..binding.operands.len())
            .map(|index| binding.operands.start + index)
            .filter(|&at| context.is_state_type(context.get_value(op.operands()[at]).ty()));
        let joined = (0..binding.results.len())
            .map(|index| binding.results.start + index)
            .filter(|&at| context.is_state_type(context.get_value(op.results()[at]).ty()));
        return forwarded
            .zip(joined)
            .map(|(entered, left)| StateChain {
                entered: op.operands()[entered],
                ports: vec![
                    region.ports()[entered - binding.operands.start + binding.ports.start].id(),
                ],
                next: None,
                exits: vec![region.results()[left - binding.results.start + binding.exit.start]],
                left: op.results()[left],
            })
            .collect();
    };
    let binding = gamma.binding();
    let arms: Vec<_> = gamma
        .arms()
        .into_iter()
        .map(|arm| context.get_region(arm))
        .collect();
    let (operands, results) = (op.operands(), op.results());
    let forwarded = (0..binding.operands.len())
        .map(|index| binding.operands.start + index)
        .filter(|&at| context.is_state_type(context.get_value(operands[at]).ty()));
    let joined = (0..binding.results.len())
        .map(|index| binding.results.start + index)
        .filter(|&at| context.is_state_type(context.get_value(results[at]).ty()));
    forwarded
        .zip(joined)
        .map(|(entered, left)| StateChain {
            entered: operands[entered],
            ports: arms
                .iter()
                .map(|arm| arm.ports()[entered - binding.operands.start + binding.ports.start].id())
                .collect(),
            next: None,
            exits: arms
                .iter()
                .map(|arm| arm.results()[left - binding.results.start + binding.exit.start])
                .collect(),
            left: results[left],
        })
        .collect()
}

/// Whether every region hands the chain's port straight back: the chain flows
/// past the operation rather than through it, since nothing under it changed
/// the memory.
pub fn forwards_state(chain: &StateChain) -> bool {
    chain.next.is_none_or(|next| next == chain.ports[0])
        && chain
            .ports
            .iter()
            .zip(&chain.exits)
            .all(|(port, exit)| port == exit)
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
    predicate: ValueId,
    arms: &[RegionId],
    binding: &Binding,
) -> Result<(), Error> {
    if arms.is_empty() {
        return Err(fail(format!("{name} needs at least one arm")));
    }
    verify_gamma_cases(context, op, name, predicate, arms.len())?;
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
    let context = op.context.clone();
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
/// each arm's label, ports and body.
pub fn print_gamma(
    fmt: &mut IRFormatter,
    op: &OpHandle,
    name: &str,
    predicate: ValueId,
    arms: &[RegionId],
    binding: &Binding,
) -> Result<(), std::fmt::Error> {
    let context = op.context.clone();
    region_format::print_result_prefix(fmt, op)?;
    fmt.write(format!("{name} %{}", predicate.number()))?;
    let inputs = slice(&op.operands(), &binding.operands);
    if !inputs.is_empty() {
        fmt.write(" args(")?;
        region_format::print_value_list(fmt, &inputs)?;
        fmt.write(")")?;
    }
    let cases = gamma_cases(op, arms.len());
    let labelled = gamma_cases_attribute(&cases).is_some();
    let width = predicate_width(&context, predicate);
    for (index, &arm) in arms.iter().enumerate() {
        if labelled {
            let label = match cases.get(index) {
                Some(&bits) => format!("case {}", signed_bits(bits, width)),
                None => "default".to_string(),
            };
            fmt.write(if fmt.at_line_start() {
                label
            } else {
                format!(" {label}")
            })?;
        }
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
    /// The [`GAMMA_CASES`] attribute, where the arms are labelled.
    pub cases: Option<AttributeValue>,
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
    let mut labels = vec![];
    loop {
        let label = if parser.parse_token("case") {
            let span = parser.span();
            let case = parser
                .parse_number()
                .ok_or((span, Error::ExpectedToken("case value")))?;
            Some(Some(case))
        } else if parser.parse_token("default") {
            Some(None)
        } else {
            None
        };
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
        } else if label.is_none() && parser.peek_char() != Some('{') {
            break;
        }
        labels.push(label);
        arms.push(parser.parse_region_with_entry_args(context, ports)?.id());
    }
    let Some(&first) = arms.first() else {
        return Err((parser.span(), Error::ExpectedToken("{")));
    };
    let cases = if labels.iter().all(Option::is_none) {
        None
    } else {
        let (default, cases) = labels.split_last().expect("an arm was parsed");
        if *default != Some(None) {
            return Err((parser.span(), Error::ExpectedToken("default")));
        }
        let width = predicate_width(context, predicate);
        let cases = cases
            .iter()
            .map(|label| match label {
                Some(Some(case)) => Ok(truncate_bits(*case, width)),
                _ => Err((parser.span(), Error::ExpectedToken("case"))),
            })
            .collect::<ParseResult<Vec<_>>>()?;
        gamma_cases_attribute(&cases)
    };
    let first = context.get_region(first);
    Ok(ParsedGamma {
        predicate,
        inputs,
        arms,
        cases,
        result_types: first
            .results()
            .iter()
            .map(|&result| context.get_value(result).ty())
            .collect(),
    })
}
