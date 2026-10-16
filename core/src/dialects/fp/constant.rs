use tir_adt::APFloat;

use super::arithmetic::float_type_parts;
use super::semantics::interp_error;

use crate::{Context, Error, Operation, operation};

use crate as tir;

operation! {
    ConstantOp {
        name: "constant",
        dialect: "fp",
        scheme: "() -> float<_, _>",
        attributes: A {
            bits: "UInt",
        },
        results: R { result },
        interfaces: [crate::ConstantLike, crate::interp::Interp, crate::Speculatable],
        verifier: "true",
    }
}

impl crate::Speculatable for ConstantOp {}

impl crate::ConstantLike for ConstantOp {
    fn constant_value(&self) -> tir_adt::APInt {
        let context = self.handle().context.clone();
        let width = float_format(&context, self.result()).map_or(64, |(.., width)| width);
        tir_adt::APInt::new(u64::from(width), self.bits())
    }
}

impl tir::Verifiable for ConstantOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let (_, _, width) = float_format(context, self.result())?;
        if width == 32 && self.bits() > u32::MAX as u64 {
            return Err(Error::VerificationError(
                "fp.constant bits do not fit binary32".into(),
            ));
        }
        Ok(())
    }
}

impl crate::interp::Interp for ConstantOp {
    fn evaluate(
        &self,
        _operands: &[crate::interp::Value],
        _state: &mut crate::interp::ExecutionState,
    ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
        let context = self.handle().context.clone();
        let (exponent, mantissa, _) =
            float_format(&context, self.result()).map_err(interp_error)?;
        Ok(vec![crate::interp::Value::Float(APFloat::from_bits(
            exponent,
            mantissa,
            false,
            self.bits() as u128,
        ))])
    }
}

fn float_format(context: &Context, value: crate::ValueId) -> Result<(u32, u32, u32), Error> {
    let invalid =
        || Error::VerificationError("fp.constant supports only binary32 and binary64".into());
    let (exponent, mantissa) =
        float_type_parts(context, context.get_value(value).ty()).map_err(|_| invalid())?;
    let width = 1 + exponent + mantissa;
    if !matches!(width, 32 | 64) {
        return Err(invalid());
    }
    Ok((exponent, mantissa, width))
}
