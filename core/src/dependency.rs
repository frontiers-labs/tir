//! Dependencies: the trailing partition of every operand, result and argument
//! list, carrying no bits and printing without a type.
//!
//! A dependency names the state of memory — or any other ordering effect — at
//! one point in the program. An operation consumes the dependencies it observes
//! and produces the ones it leaves behind, so ordering is an explicit def-use
//! edge rather than a side channel. Where the partition sits is a count on the
//! operation, block or region; what these helpers share is how it is spelled:
//! `%v | %d = op %a | %c : !ty`, the dependencies after a `|` on either side.

use crate::parse::Span;
use crate::parse::common::Cursor;
use crate::{Context, Error, IRFormatter, OpHandle, Operation, ValueId};

/// Print `%a, %b | %c, %d = ` for an op that produces anything, and nothing
/// for one that does not.
pub fn print_result_prefix(
    fmt: &mut IRFormatter<'_>,
    op: &OpHandle,
) -> Result<(), std::fmt::Error> {
    let (values, deps) = (op.value_results(), op.dep_results());
    if values.is_empty() && deps.is_empty() {
        return Ok(());
    }
    print_value_list(fmt, &values)?;
    print_dep_list(fmt, &deps, !values.is_empty())?;
    fmt.write(" = ")
}

/// Print ` | %c, %d` for an op observing any dependency, and nothing otherwise.
pub fn print_dep_operands(fmt: &mut IRFormatter<'_>, op: &OpHandle) -> Result<(), std::fmt::Error> {
    print_dep_list(fmt, &op.dep_operands(), true)
}

/// Print `| %c, %d` — led by a space when `spaced` — for a non-empty list.
pub fn print_dep_list(
    fmt: &mut IRFormatter<'_>,
    deps: &[ValueId],
    spaced: bool,
) -> Result<(), std::fmt::Error> {
    if deps.is_empty() {
        return Ok(());
    }
    fmt.write(if spaced { " | " } else { "| " })?;
    print_value_list(fmt, deps)
}

/// Print `%a, %b`.
pub fn print_value_list(
    fmt: &mut IRFormatter<'_>,
    values: &[ValueId],
) -> Result<(), std::fmt::Error> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{}", value.number()))?;
    }
    Ok(())
}

/// Parse an optional `| %c, %d` list of dependency operands.
pub fn parse_dep_operands(
    parser: &mut crate::parse::text::Parser<'_>,
    context: &Context,
) -> Result<Vec<ValueId>, (Span, Error)> {
    Ok(parse_dep_names(parser)?
        .iter()
        .map(|name| parser.resolve_value(context, name))
        .collect())
}

/// Parse an optional `| %c, %d` list, answering the names. A `|` followed by
/// no name is an error: an empty partition is spelled by leaving the `|` out.
pub fn parse_dep_names(
    parser: &mut crate::parse::text::Parser<'_>,
) -> Result<Vec<String>, (Span, Error)> {
    if !parser.parse_token("|") {
        return Ok(Vec::new());
    }
    let mut names = vec![];
    loop {
        let name = parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
        names.push(name.to_string());
        if !parser.parse_token(",") {
            return Ok(names);
        }
    }
}

/// Put `op` on the chain `observed` names and hand back the dependency it
/// leaves behind. Machine instructions built by a target's own opcode builders
/// — spill code, the stores that place a call's stack arguments — know nothing
/// of the memory they land in, so the ports are grown onto the instruction here.
pub fn put_on_chain(context: &Context, op: &dyn Operation, observed: ValueId) -> ValueId {
    context.append_dep_operand(op.id(), observed);
    context.append_dep_result(op.id())
}
