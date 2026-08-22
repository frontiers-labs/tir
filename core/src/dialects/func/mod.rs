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

fn parse_argument_alignments(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Option<AttributeValue>, (Span, Error)> {
    use tir::parse::common::Cursor;
    if !parser.parse_token("argument_alignments") {
        return Ok(None);
    }
    let value = parser
        .parse_attribute_value(context)?
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("alignment list")))?;
    if !matches!(value, AttributeValue::Array(_)) {
        return Err((parser.span(), Error::ExpectedToken("alignment list")));
    }
    Ok(Some(value))
}

fn print_argument_alignments(
    fmt: &mut IRFormatter,
    alignments: &[u64],
) -> Result<(), std::fmt::Error> {
    if alignments.is_empty() {
        return Ok(());
    }
    fmt.write(" argument_alignments [")?;
    for (index, alignment) in alignments.iter().enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        fmt.write(alignment.to_string())?;
    }
    fmt.write("]")
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
