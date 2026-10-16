mod call;
mod declare;
mod func_op;

use crate::attributes::AttributeValue;
use crate::{Context, Error, IRFormatter, Operation, dialect, parse::Span};

use crate as tir;

pub use call::*;
pub use declare::*;
pub use func_op::*;

pub mod ops {
    pub use super::call::*;
    pub use super::declare::*;
    pub use super::func_op::*;
}

dialect! {
    FuncDialect {
        name: "func",
        operations: [FuncOp, ReturnOp, CallOp, DeclareOp],
        types: [],
    }
}

fn argument_alignments(op: &impl Operation) -> Vec<u64> {
    match op.attr("argument_alignments") {
        Some(AttributeValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                AttributeValue::UInt(value) => *value,
                AttributeValue::Int(value) if *value >= 0 => *value as u64,
                _ => 0,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The arguments a function's caller guarantees name memory nothing else in the
/// function reaches: `restrict` in C, `noalias` in LLVM.
fn noalias_arguments(op: &impl Operation) -> Vec<usize> {
    match op.attr("noalias") {
        Some(AttributeValue::Array(values)) => values
            .iter()
            .filter_map(|value| match value {
                AttributeValue::UInt(value) => usize::try_from(*value).ok(),
                AttributeValue::Int(value) => usize::try_from(*value).ok(),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The `keyword [a, b, ...]` an argument list is spelled as, absent when the
/// keyword is not there. `what` names the list in the error.
fn parse_keyed_array(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
    keyword: &str,
    what: &'static str,
) -> Result<Option<AttributeValue>, (Span, Error)> {
    use tir::parse::common::Cursor;
    if !parser.parse_token(keyword) {
        return Ok(None);
    }
    let value = parser
        .parse_attribute_value(context)?
        .ok_or_else(|| (parser.span(), Error::ExpectedToken(what)))?;
    if !matches!(value, AttributeValue::Array(_)) {
        return Err((parser.span(), Error::ExpectedToken(what)));
    }
    Ok(Some(value))
}

fn print_keyed_list<T: std::fmt::Display>(
    fmt: &mut IRFormatter,
    keyword: &str,
    items: &[T],
) -> Result<(), std::fmt::Error> {
    if items.is_empty() {
        return Ok(());
    }
    fmt.write(format!(" {keyword} ["))?;
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        fmt.write(item.to_string())?;
    }
    fmt.write("]")
}

fn verify_noalias_arguments(
    context: &Context,
    op: &impl Operation,
    arguments: &[tir::TypeId],
) -> Result<(), Error> {
    let reject = |what: &str| {
        Err(Error::VerificationError(format!(
            "function noalias arguments must {what}"
        )))
    };
    let Some(attribute) = op.attr("noalias") else {
        return Ok(());
    };
    let AttributeValue::Array(entries) = attribute else {
        return reject("be an array");
    };
    for entry in entries.iter() {
        let index = match entry {
            AttributeValue::UInt(value) => usize::try_from(*value).ok(),
            AttributeValue::Int(value) => usize::try_from(*value).ok(),
            _ => None,
        };
        let Some(&ty) = index.and_then(|index| arguments.get(index)) else {
            return reject("name the function's arguments");
        };
        if crate::ptr::PtrType::view(context, ty).is_none() {
            return reject("have pointer type");
        }
    }
    Ok(())
}

fn verify_argument_alignments(
    op: &impl Operation,
    arguments: usize,
    operation: &str,
) -> Result<(), Error> {
    let Some(attribute) = op.attr("argument_alignments") else {
        return Ok(());
    };
    let AttributeValue::Array(alignments) = attribute else {
        return Err(Error::VerificationError(format!(
            "{operation} argument alignments must be an array"
        )));
    };
    if alignments.len() != arguments {
        return Err(Error::VerificationError(format!(
            "{operation} argument alignment count must match its arguments"
        )));
    }
    if alignments.iter().any(|alignment| {
        let alignment = match alignment {
            AttributeValue::UInt(value) => *value,
            AttributeValue::Int(value) if *value >= 0 => *value as u64,
            _ => return true,
        };
        !alignment.is_power_of_two()
    }) {
        return Err(Error::VerificationError(format!(
            "{operation} argument alignments must be powers of two"
        )));
    }
    Ok(())
}
