use tir_adt::{APFloat, APInt};
use tir_adt::{FloatOp, FloatWidth, RoundingMode, eval_float};

use super::{SymKind, Value};

pub(super) fn operation(kind: SymKind) -> Option<FloatOp> {
    Some(match kind {
        SymKind::FAddRound => FloatOp::Add,
        SymKind::FSubRound => FloatOp::Sub,
        SymKind::FMulRound => FloatOp::Mul,
        SymKind::FDivRound => FloatOp::Div,
        SymKind::FmaRound => FloatOp::Fma,
        SymKind::SqrtRound => FloatOp::Sqrt,
        SymKind::FCvtRound => FloatOp::Convert,
        SymKind::SIToFPRound => FloatOp::SignedToFloat,
        SymKind::UIToFPRound => FloatOp::UnsignedToFloat,
        SymKind::FPToSIRound => FloatOp::FloatToSigned,
        SymKind::FPToUIRound => FloatOp::FloatToUnsigned,
        _ => return None,
    })
}

fn width(bits: u32) -> FloatWidth {
    match bits {
        32 => FloatWidth::W32,
        64 => FloatWidth::W64,
        _ => panic!("rounded floating-point operations require 32 or 64 bits"),
    }
}

fn bits(value: Value) -> (u64, u32) {
    match value {
        Value::Int(value) => (value.to_u64(), value.width()),
        Value::Float(value) => (value.to_bits() as u64, value.bit_width()),
        _ => panic!("rounded floating-point operations require scalar operands"),
    }
}

pub(super) fn evaluate(kind: SymKind, child: &impl Fn(usize) -> Value) -> (Value, u8) {
    let op = operation(kind).expect("fp_flags requires a rounded operation");
    let rounding = match bits(child(kind.arity() - 1)).0 {
        0 => RoundingMode::TiesToEven,
        1 => RoundingMode::TowardZero,
        2 => RoundingMode::TowardNegative,
        3 => RoundingMode::TowardPositive,
        4 => RoundingMode::TiesToAway,
        _ => panic!("rounding mode must be resolved to 0..4"),
    };
    let (first, source_bits) = bits(child(0));
    let mut operands = [first, 0, 0];
    let destination_bits = match op {
        FloatOp::Convert | FloatOp::SignedToFloat | FloatOp::UnsignedToFloat => {
            1 + bits(child(1)).0 as u32 + bits(child(2)).0 as u32
        }
        FloatOp::FloatToSigned | FloatOp::FloatToUnsigned => bits(child(1)).0 as u32,
        FloatOp::Sqrt => source_bits,
        FloatOp::Add | FloatOp::Sub | FloatOp::Mul | FloatOp::Div | FloatOp::Fma => {
            operands[1] = bits(child(1)).0;
            if matches!(op, FloatOp::Fma) {
                operands[2] = bits(child(2)).0;
            }
            source_bits
        }
    };
    let result = eval_float(
        op,
        width(source_bits),
        width(destination_bits),
        operands,
        rounding,
    );
    let value = if matches!(op, FloatOp::FloatToSigned | FloatOp::FloatToUnsigned) {
        Value::Int(APInt::new(destination_bits, result.bits))
    } else {
        let (exponent, mantissa) = match destination_bits {
            32 => (8, 23),
            64 => (11, 52),
            _ => unreachable!(),
        };
        Value::Float(APFloat::from_bits(
            exponent,
            mantissa,
            false,
            result.bits as u128,
        ))
    };
    (value, result.flags)
}
