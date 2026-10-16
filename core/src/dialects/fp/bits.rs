use tir_adt::{APFloat, APInt, FloatClass, FloatWidth, classify_float};

use super::arithmetic::{float_parts, width_of_float};
use crate::{SameOperandAndResultType, Speculatable, operation};

use crate as tir;

operation! {
    NegOp {
        name: "neg", dialect: "fp", scheme: "forall E: nat, M: nat. (float<E, M>) -> float<E, M>",
        operands: O { input },
        results: R { result },
        interfaces: [SameOperandAndResultType, Speculatable, crate::interp::Interp],
    }
}

operation! {
    AbsOp {
        name: "abs", dialect: "fp", scheme: "forall E: nat, M: nat. (float<E, M>) -> float<E, M>",
        operands: O { input },
        results: R { result },
        interfaces: [SameOperandAndResultType, Speculatable, crate::interp::Interp],
    }
}

operation! {
    CopySignOp {
        name: "copysign", dialect: "fp", scheme: "forall E: nat, M: nat. (float<E, M>, float<E, M>) -> float<E, M>",
        operands: O { magnitude, sign },
        results: R { result },
        interfaces: [SameOperandAndResultType, Speculatable, crate::interp::Interp],
    }
}

operation! {
    SignBitOp {
        name: "signbit", dialect: "fp", scheme: "(float<_, _>) -> int<1>",
        operands: O { input },
        results: R { result },
        interfaces: [Speculatable, crate::interp::Interp],
    }
}

operation! {
    ClassifyOp {
        name: "classify", dialect: "fp", scheme: "(float<_, _>) -> int<1>",
        operands: O { input },
        attributes: A { class: "Str" },
        results: R { result },
        interfaces: [Speculatable, crate::interp::Interp],
        verifier: "true",
    }
}

impl SameOperandAndResultType for NegOp {}
impl SameOperandAndResultType for AbsOp {}
impl SameOperandAndResultType for CopySignOp {}
impl Speculatable for NegOp {}
impl Speculatable for AbsOp {}
impl Speculatable for CopySignOp {}
impl Speculatable for SignBitOp {}
impl Speculatable for ClassifyOp {}

impl tir::Verifiable for ClassifyOp {
    fn verify_impl(&self, _: &tir::Context) -> Result<(), tir::Error> {
        parse_class(&self.class())
            .ok_or_else(|| tir::Error::VerificationError("unknown floating-point class".into()))?;
        Ok(())
    }
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

macro_rules! unary_bits {
    ($op:ident, $apply:expr) => {
        impl crate::interp::Interp for $op {
            fn evaluate(
                &self,
                operands: &[crate::interp::Value],
                _: &mut crate::interp::ExecutionState,
            ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
                let value = float(&operands[0])?;
                let width = width_of_float(value)?;
                Ok(vec![from_bits(
                    width,
                    $apply(value.to_bits() as u64, sign_mask(width)),
                )])
            }
        }
    };
}

unary_bits!(NegOp, |bits: u64, sign: u64| bits ^ sign);
unary_bits!(AbsOp, |bits: u64, sign: u64| bits & !sign);

impl crate::interp::Interp for CopySignOp {
    fn evaluate(
        &self,
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
}

impl crate::interp::Interp for SignBitOp {
    fn evaluate(
        &self,
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
}

impl crate::interp::Interp for ClassifyOp {
    fn evaluate(
        &self,
        operands: &[crate::interp::Value],
        _: &mut crate::interp::ExecutionState,
    ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
        let value = float(&operands[0])?;
        let width = width_of_float(value)?;
        let class = parse_class(&self.class()).expect("verified class");
        Ok(vec![crate::interp::Value::Int(APInt::new(
            1,
            u64::from(classify_float(width, value.to_bits() as u64) == class),
        ))])
    }
}
