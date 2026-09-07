//! What a `binds:` declaration means at run time: the ranges an op's
//! [`Binding`] names, the checks that hold them aligned, and the one syntax
//! every declared theta and gamma shares.
//!
//! A theta prints as `%r = dialect.op (%port = %init, .. | %dport = %dinit) { .. }`
//! and a gamma as `%r = dialect.op %pred args(%in, .. | %d) (%port | %dport) { .. } (..) { .. }`.
//! Types are not spelled: a port has its init's type, a theta result its
//! init's, and a gamma result the type of the first arm's result.

use std::ops::Range;

use crate::attributes::{AttributeValue, Predicate};
use crate::builtin::{AddIOp, CmpIOp, IntegerType};
use crate::parse::Span;
use crate::parse::common::Cursor;
use crate::parse::text::Parser;
use crate::{
    Binding, Context, Error, IRFormatter, OpHandle, RegionId, TypeId, ValueId, dependency,
    region_format,
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

/// How many value ports (or value results) the op's `index`-th region has;
/// zero for a region the op does not hold, so a gamma with no arms still
/// answers.
pub fn region_list_len(context: &Context, op: &OpHandle, index: usize, ports: bool) -> usize {
    let Some(&region) = op.regions().get(index) else {
        return 0;
    };
    let region = context.get_region(region);
    if ports {
        region.value_arguments().len()
    } else {
        region.value_results().len()
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

/// Checks a theta's declared alignment: five ranges of one length, one type per
/// offset, a boolean predicate, and dependencies carried in the same shape.
pub fn verify_theta(
    context: &Context,
    op: &OpHandle,
    name: &str,
    body: RegionId,
    binding: &Binding,
    predicate: usize,
) -> Result<(), Error> {
    let region = context.get_region(body);
    let (ports, results) = (region.value_arguments(), region.value_results());
    let ports: Vec<ValueId> = ports.iter().map(crate::Value::id).collect();
    let (operands, op_results) = (op.value_operands(), op.value_results());
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
    )?;

    let m = op.dep_operands().len();
    let dep_counts = [
        (region.dep_arguments().len(), m, "dependency ports"),
        (region.dep_results().len(), 2 * m, "dependency body results"),
        (op.dep_results().len(), m, "dependency results"),
    ];
    for (found, expected, what) in dep_counts {
        if found != expected {
            return Err(fail(format!(
                "{name} carries {m} dependencies but has {found} {what}, not {expected}"
            )));
        }
    }
    Ok(())
}

/// Checks a gamma's declared alignment on every arm: ports typed like the
/// forwarded operands, results typed like the op's, dependencies alike. An
/// arm's port and result lists are aligned whole, so each arm is read in
/// full rather than through the ranges arm 0 gave the binding.
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
    let inputs = slice(&op.value_operands(), &binding.operands);
    let results = slice(&op.value_results(), &binding.results);
    let (dep_inputs, dep_results) = (op.dep_operands().len(), op.dep_results().len());
    for (index, &arm) in arms.iter().enumerate() {
        let region = context.get_region(arm);
        let ports: Vec<ValueId> = region
            .value_arguments()
            .iter()
            .map(crate::Value::id)
            .collect();
        let produced = region.value_results();
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
        let (dep_ports, dep_produced) = (region.dep_arguments().len(), region.dep_results().len());
        if dep_ports != dep_inputs || dep_produced != dep_results {
            return Err(fail(format!(
                "{arm_name} carries {dep_ports} dependencies in and {dep_produced} out, but the op forwards {dep_inputs} and produces {dep_results}"
            )));
        }
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
    let ports: Vec<ValueId> = region
        .value_arguments()
        .iter()
        .map(crate::Value::id)
        .collect();
    let results = region.value_results();
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
    let dep_results = region.dep_results();
    let dep_ports = region.dep_arguments();
    for (index, (exit, port)) in dep_results[dep_results.len() / 2..]
        .iter()
        .zip(&dep_ports)
        .enumerate()
    {
        if *exit != port.id() {
            return Err(fail(format!(
                "{name} exit dependency {index} must be dependency port {index}"
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

/// Print `(%port = %init, .. | %dport = %dinit, ..)`, or nothing when the op
/// carries nothing.
pub fn print_port_bindings(
    fmt: &mut IRFormatter,
    ports: &[ValueId],
    inits: &[ValueId],
    dep_ports: &[ValueId],
    dep_inits: &[ValueId],
) -> Result<(), std::fmt::Error> {
    if ports.is_empty() && dep_ports.is_empty() {
        return Ok(());
    }
    fmt.write(" (")?;
    print_pairs(fmt, ports, inits)?;
    if !dep_ports.is_empty() {
        fmt.write(if ports.is_empty() { "| " } else { " | " })?;
        print_pairs(fmt, dep_ports, dep_inits)?;
    }
    fmt.write(")")
}

fn port_ids(context: &Context, region: RegionId) -> (Vec<ValueId>, Vec<ValueId>) {
    let region = context.get_region(region);
    (
        region
            .value_arguments()
            .iter()
            .map(crate::Value::id)
            .collect(),
        region
            .dep_arguments()
            .iter()
            .map(crate::Value::id)
            .collect(),
    )
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
    dependency::print_result_prefix(fmt, op)?;
    fmt.write(name)?;
    let (ports, dep_ports) = port_ids(&context, body);
    print_port_bindings(
        fmt,
        &slice(&ports, &binding.ports),
        &slice(&op.value_operands(), &binding.operands),
        &dep_ports,
        &op.dep_operands(),
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
    dependency::print_result_prefix(fmt, op)?;
    fmt.write(format!("{name} %{}", predicate.number()))?;
    let inputs = slice(&op.value_operands(), &binding.operands);
    let dep_inputs = op.dep_operands();
    if !inputs.is_empty() || !dep_inputs.is_empty() {
        fmt.write(" args(")?;
        dependency::print_value_list(fmt, &inputs)?;
        dependency::print_dep_list(fmt, &dep_inputs, !inputs.is_empty())?;
        fmt.write(")")?;
    }
    for &arm in arms {
        let (ports, dep_ports) = port_ids(&context, arm);
        let ports = slice(&ports, &binding.ports);
        if !ports.is_empty() || !dep_ports.is_empty() {
            fmt.write(if fmt.at_line_start() { "(" } else { " (" })?;
            dependency::print_value_list(fmt, &ports)?;
            dependency::print_dep_list(fmt, &dep_ports, !ports.is_empty())?;
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
    pub dep_inits: Vec<ValueId>,
    pub body: RegionId,
    pub result_types: Vec<TypeId>,
}

/// What the generic gamma syntax names.
pub struct ParsedGamma {
    pub predicate: ValueId,
    pub inputs: Vec<ValueId>,
    pub dep_inputs: Vec<ValueId>,
    pub arms: Vec<RegionId>,
    pub result_types: Vec<TypeId>,
    pub dep_results: usize,
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

/// Mint a port named `name` with the type `init` has (a dependency's for a
/// dependency) and bind the name to it.
fn bind_port(parser: &mut Parser, context: &Context, name: &str, ty: TypeId) -> crate::Value {
    let port = context.create_value(ty, None);
    parser.define_value(name, port.id());
    port
}

/// The ports a `(%port = %init, .. | %dport = %dinit, ..)` clause binds, each
/// minted with its init's type, and the inits they are bound to.
#[derive(Default)]
pub struct PortBindings {
    pub ports: Vec<crate::Value>,
    pub dep_ports: Vec<crate::Value>,
    pub inits: Vec<ValueId>,
    pub dep_inits: Vec<ValueId>,
}

/// Parse an optional `(%port = %init, .. | %dport = %dinit, ..)` clause.
pub fn parse_port_bindings(parser: &mut Parser, context: &Context) -> ParseResult<PortBindings> {
    let mut bound = PortBindings::default();
    if parser.parse_token("(") {
        let mut deps = false;
        loop {
            if !deps && parser.parse_token("|") {
                deps = true;
            }
            let name = parser
                .parse_value_ref()
                .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
                .to_string();
            expect(parser, "=")?;
            let init = value(parser, context)?;
            if deps {
                bound
                    .dep_ports
                    .push(bind_port(parser, context, &name, TypeId::DEPENDENCY));
                bound.dep_inits.push(init);
            } else {
                let ty = context.get_value(init).ty();
                bound.ports.push(bind_port(parser, context, &name, ty));
                bound.inits.push(init);
            }
            if !parser.parse_token(",") && !(!deps && parser.peek_char() == Some('|')) {
                break;
            }
        }
        expect(parser, ")")?;
    }
    Ok(bound)
}

/// Parse the generic theta syntax after its mnemonic.
pub fn parse_theta(parser: &mut Parser, context: &Context) -> ParseResult<ParsedTheta> {
    let bound = parse_port_bindings(parser, context)?;
    let result_types = bound.ports.iter().map(crate::Value::ty).collect();
    let body = parser
        .parse_region_with_entry_args_and_deps(context, bound.ports, bound.dep_ports)?
        .id();
    Ok(ParsedTheta {
        inits: bound.inits,
        dep_inits: bound.dep_inits,
        body,
        result_types,
    })
}

/// Parse the generic gamma syntax after its mnemonic.
pub fn parse_gamma(parser: &mut Parser, context: &Context) -> ParseResult<ParsedGamma> {
    let predicate = value(parser, context)?;
    let (mut inputs, mut dep_inputs) = (vec![], vec![]);
    if parser.parse_token("args") {
        expect(parser, "(")?;
        while parser.peek_char() == Some('%') {
            inputs.push(value(parser, context)?);
            if !parser.parse_token(",") {
                break;
            }
        }
        dep_inputs = dependency::parse_dep_operands(parser, context)?;
        expect(parser, ")")?;
    }
    let mut arms = vec![];
    loop {
        let (mut ports, mut dep_ports) = (vec![], vec![]);
        if parser.parse_token("(") {
            for &input in &inputs {
                if !ports.is_empty() {
                    expect(parser, ",")?;
                }
                let name = parser
                    .parse_value_ref()
                    .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
                    .to_string();
                let ty = context.get_value(input).ty();
                ports.push(bind_port(parser, context, &name, ty));
            }
            for name in dependency::parse_dep_names(parser)? {
                dep_ports.push(bind_port(parser, context, &name, TypeId::DEPENDENCY));
            }
            expect(parser, ")")?;
        } else if parser.peek_char() != Some('{') {
            break;
        }
        arms.push(
            parser
                .parse_region_with_entry_args_and_deps(context, ports, dep_ports)?
                .id(),
        );
    }
    let Some(&first) = arms.first() else {
        return Err((parser.span(), Error::ExpectedToken("{")));
    };
    let first = context.get_region(first);
    Ok(ParsedGamma {
        predicate,
        inputs,
        dep_inputs,
        arms,
        result_types: first
            .value_results()
            .iter()
            .map(|&result| context.get_value(result).ty())
            .collect(),
        dep_results: first.dep_results().len(),
    })
}
