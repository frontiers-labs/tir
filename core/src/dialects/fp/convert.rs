use tir_adt::{APFloat, APInt, FloatOp, FloatWidth, eval_float};

use super::arithmetic::{
    canonicalize_nan, float_parts, float_width, rounding_node, value_operation, verify_policy,
    width_of_float,
};
use super::resource::{apply_flags, effect_records, effects_for, verify_ports};
use super::{ArithmeticSemantics, IntegerConversionSemantics, Rounding, SubnormalMode};
use crate::builtin::{FloatType, IntegerType};
use crate::{
    Context, Error, HasResourceSemantics, ResourceEffect, ResourceEffects, ResourceSemantics,
    TypeId, operation,
};

use super::semantics::{interp_error, speculatable_if_ignore};
use crate as tir;

fn int_width(context: &Context, ty: TypeId) -> Result<FloatWidth, Error> {
    let data = context.get_type_data(ty);
    let integer = (data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()
        .ok_or_else(|| Error::VerificationError("expected integer type".into()))?;
    match integer.width() {
        32 => Ok(FloatWidth::W32),
        64 => Ok(FloatWidth::W64),
        _ => Err(Error::VerificationError(
            "FP integer conversions support only i32 and i64".into(),
        )),
    }
}

fn width_node(graph: &mut tir::sem::SemGraph, value: u32) -> tir::NodeId {
    let width = (u32::BITS - value.leading_zeros()).max(1);
    let node = graph.add_node(tir::sem::SymKind::Constant);
    graph.set_leaf_data(
        node,
        tir::sem::SymPayload::Int(APInt::new(width, value as u64)),
    );
    node
}

fn verify_arithmetic(
    context: &Context,
    op: &tir::OpHandle,
    semantics: &ArithmeticSemantics,
) -> Result<(), Error> {
    verify_policy(semantics)?;
    float_width(context, context.get_value(op.value_results()[0]).ty())?;
    verify_ports(
        op,
        &effects_for(Some(semantics.rounding), semantics.exceptions),
    )
}

fn rounding(value: Rounding, state: &crate::interp::ExecutionState) -> tir_adt::RoundingMode {
    match value {
        Rounding::Fixed(mode) => mode,
        Rounding::Dynamic => state.fp_environment.rounding,
    }
}

fn input_node(graph: &mut tir::sem::SemGraph) -> tir::NodeId {
    let node = graph.add_node(tir::sem::SymKind::Symbol);
    graph.set_leaf_data(node, tir::sem::SymPayload::SymbolId(0));
    node
}

fn arithmetic_conversion_semantics(
    op: &tir::OpHandle,
    graph: &mut tir::sem::SemGraph,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
    semantics: &ArithmeticSemantics,
) -> Option<tir::NodeId> {
    let nearest_any_quiet = semantics.nan == super::NaNPolicy::AnyQuiet
        && semantics.rounding == Rounding::Fixed(tir_adt::RoundingMode::TiesToEven);
    let input = input_node(graph);
    let result_type = op.context.get_value(op.value_results()[0]).ty();
    let result_width = float_width(&op.context, result_type).ok()?;
    let (exponent_width, mantissa_width) = float_parts(result_width);
    let exponent = width_node(graph, exponent_width);
    let mantissa = width_node(graph, mantissa_width);
    let rounding = if nearest_any_quiet {
        None
    } else {
        Some(rounding_node(graph, semantics.rounding)?)
    };
    let value = value_operation(
        graph,
        if nearest_any_quiet { generic } else { rounded },
        &[input, exponent, mantissa],
        rounding,
    );
    if semantics.nan != super::NaNPolicy::Canonical {
        return Some(value);
    }
    let bits = super::resource::constant(
        graph,
        1 + exponent_width + mantissa_width,
        super::arithmetic::canonical_nan(result_width),
    );
    Some(super::resource::canonicalize_nan(graph, value, bits))
}

fn integer_conversion_semantics(
    op: &tir::OpHandle,
    graph: &mut tir::sem::SemGraph,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
    semantics: &IntegerConversionSemantics,
) -> Option<tir::NodeId> {
    let toward_zero = semantics.rounding == Rounding::Fixed(tir_adt::RoundingMode::TowardZero);
    let result_type = op.context.get_value(op.value_results()[0]).ty();
    let data = op.context.get_type_data(result_type);
    let integer = (data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()
        .expect("verified integer conversion result");
    let input = input_node(graph);
    let width = width_node(graph, integer.width());
    let rounding = if toward_zero {
        None
    } else {
        Some(rounding_node(graph, semantics.rounding)?)
    };
    Some(value_operation(
        graph,
        if toward_zero { generic } else { rounded },
        &[input, width],
        rounding,
    ))
}

macro_rules! arithmetic_conversion {
    (float, $op:ident, $name:tt, $kind:expr, $generic:expr, $rounded:expr) => {
        arithmetic_conversion!(@impl $op, $name, "FloatType", float_width, $kind, $generic, $rounded);
    };
    (integer, $op:ident, $name:tt, $kind:expr, $generic:expr, $rounded:expr) => {
        arithmetic_conversion!(@impl $op, $name, "IntegerType", int_width, $kind, $generic, $rounded);
    };
    (@impl $op:ident, $name:tt, $input_type:tt, $source_width:ident, $kind:expr, $generic:expr, $rounded:expr) => {
        operation! {
            $op {
                name: $name, dialect: "fp",
                operands: O { input: $input_type }, attributes: A { semantics: "FpSemantics" },
                results: R { result: "FloatType" }, interfaces: [ResourceEffects, HasResourceSemantics, crate::Speculatable, crate::interp::Interp],
                sem: "(set result $value_semantics)",
                state: "in_out", verifier: "true",
            }
        }

        impl $op {
            fn value_semantics(
                &self,
                graph: &mut tir::sem::SemGraph,
            ) -> Option<tir::NodeId> {
                let semantics = self.semantics();
                arithmetic_conversion_semantics(
                    &self.0,
                    graph,
                    $generic,
                    $rounded,
                    semantics.arithmetic().ok()?,
                )
            }
        }

        impl tir::Verifiable for $op {
            fn verify_impl(&self, context: &Context) -> Result<(), Error> {
                let semantics = self.semantics();
                let semantics = semantics.arithmetic()?;
                $source_width(context, context.get_value(self.0.value_operands()[0]).ty())?;
                verify_arithmetic(context, &self.0, semantics)
            }
        }
        impl crate::Speculatable for $op {
            fn is_speculatable(&self) -> bool {
                speculatable_if_ignore(self.semantics().arithmetic().map(|s| s.exceptions))
            }
        }

        impl ResourceEffects for $op {
            fn resource_effects(&self) -> Vec<ResourceEffect> {
                let semantics = self.semantics();
                let semantics = semantics.arithmetic().expect("verified semantics");
                effect_records(&self.0, effects_for(Some(semantics.rounding), semantics.exceptions))
            }
        }
        impl HasResourceSemantics for $op {
            fn resource_semantics(&self) -> ResourceSemantics {
                let semantics = self.semantics();
                let result_type = self.0.context.get_value(self.0.value_results()[0]).ty();
                let format = float_parts(
                    float_width(&self.0.context, result_type)
                        .expect("verified floating conversion result"),
                );
                super::resource::arithmetic(
                    &self.0,
                    $generic,
                    $rounded,
                    Some(format),
                    semantics
                        .arithmetic()
                        .expect("verified arithmetic conversion semantics"),
                )
            }
        }
        impl crate::interp::Interp for $op {
            fn evaluate(
                &self,
                operands: &[crate::interp::Value],
                state: &mut crate::interp::ExecutionState,
            ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
                let semantics = self.semantics();
                let semantics = semantics
                    .arithmetic()
                    .map_err(interp_error)?;
                let source_type = self.0.context.get_value(self.0.value_operands()[0]).ty();
                let destination_type = self.0.context.get_value(self.0.value_results()[0]).ty();
                let source = $source_width(&self.0.context, source_type).map_err(interp_error)?;
                let destination = float_width(&self.0.context, destination_type)
                    .map_err(interp_error)?;
                let bits = match &operands[0] {
                    crate::interp::Value::Float(value) => value.to_bits() as u64,
                    crate::interp::Value::Int(value) => value.to_u64(),
                    _ => {
                        return Err(crate::interp::InterpError::Message(
                            "invalid FP conversion operand".into(),
                        ));
                    }
                };
                let result = apply_flags(
                    eval_float(
                        $kind,
                        source,
                        destination,
                        [bits, 0, 0],
                        rounding(semantics.rounding, state),
                    ),
                    semantics.exceptions,
                    state,
                )?;
                let bits = match semantics.nan {
                    super::NaNPolicy::Canonical => canonicalize_nan(destination, result.bits),
                    _ => result.bits,
                };
                let (exponent, mantissa) = float_parts(destination);
                Ok(vec![crate::interp::Value::Float(APFloat::from_bits(
                    exponent,
                    mantissa,
                    false,
                    bits as u128,
                ))])
            }
        }
    };
}

arithmetic_conversion!(
    float,
    ConvertOp,
    "convert",
    FloatOp::Convert,
    tir::sem::SymKind::FCvt,
    tir::sem::SymKind::FCvtRound
);
arithmetic_conversion!(
    integer,
    FromSiOp,
    "from_si",
    FloatOp::SignedToFloat,
    tir::sem::SymKind::SIToFP,
    tir::sem::SymKind::SIToFPRound
);
arithmetic_conversion!(
    integer,
    FromUiOp,
    "from_ui",
    FloatOp::UnsignedToFloat,
    tir::sem::SymKind::UIToFP,
    tir::sem::SymKind::UIToFPRound
);

macro_rules! to_integer {
    ($op:ident, $name:tt, $kind:expr, $generic:expr, $rounded:expr) => {
        operation! {
            $op {
                name: $name, dialect: "fp",
                operands: O { input: "FloatType" }, attributes: A { semantics: "FpSemantics" },
                results: R { result: "IntegerType" }, interfaces: [ResourceEffects, HasResourceSemantics, crate::Speculatable, crate::interp::Interp],
                sem: "(set result $value_semantics)",
                state: "in_out", verifier: "true",
            }
        }

        impl $op {
            fn value_semantics(
                &self,
                graph: &mut tir::sem::SemGraph,
            ) -> Option<tir::NodeId> {
                let semantics = self.semantics();
                integer_conversion_semantics(
                    &self.0,
                    graph,
                    $generic,
                    $rounded,
                    semantics.integer_conversion().ok()?,
                )
            }
        }

        impl tir::Verifiable for $op {
            fn verify_impl(&self, context: &Context) -> Result<(), Error> {
                float_width(context, context.get_value(self.0.value_operands()[0]).ty())?;
                int_width(context, context.get_value(self.result()).ty())?;
                let semantics = self.semantics();
                let semantics = semantics.integer_conversion()?;
                if semantics.subnormals != SubnormalMode::Gradual {
                    return Err(Error::VerificationError(
                        "FP conversion supports gradual subnormals".into(),
                    ));
                }
                verify_ports(&self.0, &effects_for(Some(semantics.rounding), semantics.exceptions))
            }
        }
        impl crate::Speculatable for $op {
            fn is_speculatable(&self) -> bool {
                speculatable_if_ignore(self.semantics().integer_conversion().map(|s| s.exceptions))
            }
        }
        impl ResourceEffects for $op {
            fn resource_effects(&self) -> Vec<ResourceEffect> {
                let semantics = self.semantics();
                let semantics = semantics.integer_conversion().expect("verified semantics");
                effect_records(&self.0, effects_for(Some(semantics.rounding), semantics.exceptions))
            }
        }
        impl HasResourceSemantics for $op {
            fn resource_semantics(&self) -> ResourceSemantics {
                let semantics = self.semantics();
                super::resource::integer_conversion(
                    &self.0,
                    $generic,
                    $rounded,
                    semantics
                        .integer_conversion()
                        .expect("verified integer conversion semantics"),
                )
            }
        }
        impl crate::interp::Interp for $op {
            fn evaluate(
                &self,
                operands: &[crate::interp::Value],
                state: &mut crate::interp::ExecutionState,
            ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
                let crate::interp::Value::Float(value) = &operands[0] else {
                    return Err(crate::interp::InterpError::Message(
                        "FP-to-integer conversion requires a float".into(),
                    ));
                };
                let semantics = self.semantics();
                let semantics = semantics
                    .integer_conversion()
                    .map_err(interp_error)?;
                let source = width_of_float(value)?;
                let destination = int_width(
                    &self.0.context,
                    self.0.context.get_value(self.result()).ty(),
                )
                .map_err(interp_error)?;
                let result = apply_flags(
                    eval_float(
                        $kind,
                        source,
                        destination,
                        [value.to_bits() as u64, 0, 0],
                        rounding(semantics.rounding, state),
                    ),
                    semantics.exceptions,
                    state,
                )?;
                let width = destination.bit_width();
                Ok(vec![crate::interp::Value::Int(APInt::new(
                    width,
                    result.bits,
                ))])
            }
        }
    };
}

to_integer!(
    ToSiOp,
    "to_si",
    FloatOp::FloatToSigned,
    tir::sem::SymKind::FPToSI,
    tir::sem::SymKind::FPToSIRound
);
to_integer!(
    ToUiOp,
    "to_ui",
    FloatOp::FloatToUnsigned,
    tir::sem::SymKind::FPToUI,
    tir::sem::SymKind::FPToUIRound
);
