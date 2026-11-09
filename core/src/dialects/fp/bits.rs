use tir_adt::{APFloat, APInt, FloatClass, FloatWidth, classify_float};

use super::arithmetic::{float_parts, width_of_float};
use crate::{SameOperandAndResultType, Speculatable};

use crate as tir;

tir::helpers::op_def! {
    file: "defs/converted/dialects_fp_bits.cdsl",
    hosts: {
        neg_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        neg_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        neg_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, operands, _state| unary_bits_evaluate(operands, |bits, sign| bits ^ sign) }),
        abs_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        abs_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        abs_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, operands, _state| unary_bits_evaluate(operands, |bits, sign| bits & !sign) }),
        copysign_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        copysign_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        copysign_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_CopySignOp_Interp),
        signbit_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        signbit_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_SignBitOp_Interp),
        classify_verifier => crate::fp::classify_verify_impl,
        classify_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        classify_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_ClassifyOp_Interp),
    },
}

pub(crate) fn classify_verify_impl(
    operation: &tir::OpHandle,
    _: &tir::Context,
) -> Result<(), tir::Error> {
    parse_class(&crate::fp::classify_class(operation))
        .ok_or_else(|| tir::Error::VerificationError("unknown floating-point class".into()))?;
    Ok(())
}

fn parse_class(name: &str) -> Option<FloatClass> {
    Some(match name {
        "signaling_nan" => FloatClass::SignalingNaN,
        "quiet_nan" => FloatClass::QuietNaN,
        "negative_infinity" => FloatClass::NegativeInfinity,
        "negative_normal" => FloatClass::NegativeNormal,
        "negative_subnormal" => FloatClass::NegativeSubnormal,
        "negative_zero" => FloatClass::NegativeZero,
        "positive_zero" => FloatClass::PositiveZero,
        "positive_subnormal" => FloatClass::PositiveSubnormal,
        "positive_normal" => FloatClass::PositiveNormal,
        "positive_infinity" => FloatClass::PositiveInfinity,
        _ => return None,
    })
}

fn float(value: &crate::interp::Value) -> Result<&APFloat, crate::interp::InterpError> {
    let crate::interp::Value::Float(value) = value else {
        return Err(crate::interp::InterpError::Message(
            "floating bit operation requires a float".into(),
        ));
    };
    Ok(value)
}

fn sign_mask(width: FloatWidth) -> u64 {
    match width {
        FloatWidth::W32 => 1 << 31,
        FloatWidth::W64 => 1 << 63,
    }
}

fn from_bits(width: FloatWidth, bits: u64) -> crate::interp::Value {
    let (exponent, mantissa) = float_parts(width);
    crate::interp::Value::Float(APFloat::from_bits(exponent, mantissa, false, bits as u128))
}

fn unary_bits_evaluate(
    operands: &[crate::interp::Value],
    apply: fn(u64, u64) -> u64,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let value = float(&operands[0])?;
    let width = width_of_float(value)?;
    Ok(vec![from_bits(
        width,
        apply(value.to_bits() as u64, sign_mask(width)),
    )])
}

pub(crate) fn copysign_interp_evaluate(
    __operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    _: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let magnitude = float(&operands[0])?;
    let sign = float(&operands[1])?;
    let width = width_of_float(magnitude)?;
    if width_of_float(sign)? != width {
        return Err(crate::interp::InterpError::Message(
            "fp.copysign operands have different formats".into(),
        ));
    }
    let mask = sign_mask(width);
    let bits = (magnitude.to_bits() as u64 & !mask) | (sign.to_bits() as u64 & mask);
    Ok(vec![from_bits(width, bits)])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CopySignOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::copysign_interp_evaluate,
    };

pub(crate) fn signbit_interp_evaluate(
    __operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    _: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let value = float(&operands[0])?;
    let width = width_of_float(value)?;
    Ok(vec![crate::interp::Value::Int(APInt::new(
        1,
        u64::from(value.to_bits() as u64 & sign_mask(width) != 0),
    ))])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SignBitOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::signbit_interp_evaluate,
    };

pub(crate) fn classify_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    _: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let value = float(&operands[0])?;
    let width = width_of_float(value)?;
    let class = parse_class(&crate::fp::classify_class(operation)).expect("verified class");
    Ok(vec![crate::interp::Value::Int(APInt::new(
        1,
        u64::from(classify_float(width, value.to_bits() as u64) == class),
    ))])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_ClassifyOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::classify_interp_evaluate,
    };
