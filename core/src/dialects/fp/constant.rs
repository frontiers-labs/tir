use tir_adt::APFloat;

use super::arithmetic::float_type_parts;
use super::semantics::interp_error;

use crate::{Context, Error, Operation};

use crate as tir;

tir::helpers::op_def! {
    file: "defs/converted/dialects_fp_constant.cdsl",
    hosts: {
        constant_verifier => crate::fp::constant_verify_impl,
        constant_iface_ConstantLike => (crate :: ConstantLike, &crate::fp::__TIR_IFACE_ConstantOp_ConstantLike),
        constant_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_ConstantOp_Interp),
        constant_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
    },
}

pub(crate) fn constant_constantlike_constant_value(operation: &tir::OpHandle) -> tir_adt::APInt {
    let context = operation.handle().context.clone();
    let width = float_format(&context, crate::fp::constant_result(operation))
        .map_or(64, |(.., width)| width);
    tir_adt::APInt::new(u64::from(width), crate::fp::constant_bits(operation))
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_ConstantOp_ConstantLike: crate::ConstantLikeCallbacks =
    crate::ConstantLikeCallbacks {
        constant_value: crate::fp::constant_constantlike_constant_value,
    };

pub(crate) fn constant_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    let (_, _, width) = float_format(context, crate::fp::constant_result(operation))?;
    if width == 32 && crate::fp::constant_bits(operation) > u32::MAX as u64 {
        return Err(Error::VerificationError(
            "fp.constant bits do not fit binary32".into(),
        ));
    }
    Ok(())
}

pub(crate) fn constant_interp_evaluate(
    operation: &tir::OpHandle,
    _operands: &[crate::interp::Value],
    _state: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let context = operation.handle().context.clone();
    let (exponent, mantissa, _) =
        float_format(&context, crate::fp::constant_result(operation)).map_err(interp_error)?;
    Ok(vec![crate::interp::Value::Float(APFloat::from_bits(
        exponent,
        mantissa,
        false,
        crate::fp::constant_bits(operation) as u128,
    ))])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_ConstantOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::constant_interp_evaluate,
    };

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
