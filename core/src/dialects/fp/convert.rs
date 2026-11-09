use tir_adt::{APFloat, APInt, FloatOp, FloatWidth, eval_float};

use super::arithmetic::{
    canonicalize_nan, float_parts, float_width, rounding_node, value_operation, verify_policy,
    width_of_float,
};
use super::resource::{apply_flags, effect_records, effects_for, verify_ports};
use super::{ArithmeticSemantics, IntegerConversionSemantics, Rounding, SubnormalMode};
use crate::builtin::IntegerType;
use crate::{
    Context, Error, HasResourceSemantics, ResourceEffect, ResourceEffects, ResourceSemantics,
    TypeId,
};

use super::semantics::{interp_error, speculatable_if_ignore};
use crate as tir;

fn int_width(context: &Context, ty: TypeId) -> Result<FloatWidth, Error> {
    let integer = IntegerType::view(context, ty)
        .ok_or_else(|| Error::VerificationError("expected integer type".into()))?;
    match integer.width() {
        32 => Ok(FloatWidth::W32),
        64 => Ok(FloatWidth::W64),
        _ => Err(Error::VerificationError(
            "FP integer conversions support only i32 and i64".into(),
        )),
    }
}

fn width_node(graph: &mut tir::sem::SemGraph, value: u64) -> tir::NodeId {
    let width = u64::from(u64::BITS - value.leading_zeros()).max(1);
    let node = graph.add_node(tir::sem::SymKind::Constant);
    graph.set_leaf_data(node, tir::sem::SymPayload::Int(APInt::new(width, value)));
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
    let exponent = width_node(graph, u64::from(exponent_width));
    let mantissa = width_node(graph, u64::from(mantissa_width));
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
    let integer =
        IntegerType::view(&op.context, result_type).expect("verified integer conversion result");
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

tir::helpers::op_def! {
    file: "defs/converted/dialects_fp_convert.cdsl",
    hosts: {
        convert_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_conversion_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::FCvt,
                        tir::sem::SymKind::FCvtRound,
                    ),
                    _ => None,
                },
            )
        },
        convert_verifier => |operation, context| arithmetic_conversion_verifiable_verify_impl(operation, context, float_width ),
        convert_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_conversion_resourceeffects_resource_effects(operation) }),
        convert_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_conversion_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::FCvt , tir::sem::SymKind::FCvtRound ) }),
        convert_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_conversion_speculatable_is_speculatable) }),
        convert_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_conversion_interp_evaluate(operation, operands, state, FloatOp::Convert , float_width ) }),
        from_si_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_conversion_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::SIToFP,
                        tir::sem::SymKind::SIToFPRound,
                    ),
                    _ => None,
                },
            )
        },
        from_si_verifier => |operation, context| arithmetic_conversion_verifiable_verify_impl(operation, context, int_width ),
        from_si_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_conversion_resourceeffects_resource_effects(operation) }),
        from_si_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_conversion_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::SIToFP , tir::sem::SymKind::SIToFPRound ) }),
        from_si_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_conversion_speculatable_is_speculatable) }),
        from_si_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_conversion_interp_evaluate(operation, operands, state, FloatOp::SignedToFloat , int_width ) }),
        from_ui_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_conversion_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::UIToFP,
                        tir::sem::SymKind::UIToFPRound,
                    ),
                    _ => None,
                },
            )
        },
        from_ui_verifier => |operation, context| arithmetic_conversion_verifiable_verify_impl(operation, context, int_width ),
        from_ui_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_conversion_resourceeffects_resource_effects(operation) }),
        from_ui_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_conversion_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::UIToFP , tir::sem::SymKind::UIToFPRound ) }),
        from_ui_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_conversion_speculatable_is_speculatable) }),
        from_ui_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_conversion_interp_evaluate(operation, operands, state, FloatOp::UnsignedToFloat , int_width ) }),
        to_si_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => to_integer_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::FPToSI,
                        tir::sem::SymKind::FPToSIRound,
                    ),
                    _ => None,
                },
            )
        },
        to_si_verifier => to_integer_verifiable_verify_impl,
        to_si_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| to_integer_resourceeffects_resource_effects(operation) }),
        to_si_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| to_integer_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::FPToSI , tir::sem::SymKind::FPToSIRound ) }),
        to_si_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(to_integer_speculatable_is_speculatable) }),
        to_si_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| to_integer_interp_evaluate(operation, operands, state, FloatOp::FloatToSigned ) }),
        to_ui_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => to_integer_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::FPToUI,
                        tir::sem::SymKind::FPToUIRound,
                    ),
                    _ => None,
                },
            )
        },
        to_ui_verifier => to_integer_verifiable_verify_impl,
        to_ui_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| to_integer_resourceeffects_resource_effects(operation) }),
        to_ui_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| to_integer_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::FPToUI , tir::sem::SymKind::FPToUIRound ) }),
        to_ui_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(to_integer_speculatable_is_speculatable) }),
        to_ui_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| to_integer_interp_evaluate(operation, operands, state, FloatOp::FloatToUnsigned ) }),
    },
}

fn arithmetic_conversion_value_semantics(
    operation: &tir::OpHandle,
    graph: &mut tir::sem::SemGraph,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
) -> Option<tir::NodeId> {
    let semantics = super::semantics::operation_semantics(operation);
    arithmetic_conversion_semantics(
        operation,
        graph,
        generic,
        rounded,
        semantics.arithmetic().ok()?,
    )
}

fn arithmetic_conversion_verifiable_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
    source_width: fn(&Context, TypeId) -> Result<FloatWidth, Error>,
) -> Result<(), Error> {
    let semantics = super::semantics::operation_semantics(operation);
    let semantics = semantics.arithmetic()?;
    source_width(
        context,
        context.get_value(operation.value_operands()[0]).ty(),
    )?;
    verify_arithmetic(context, operation, semantics)
}

fn arithmetic_conversion_speculatable_is_speculatable(operation: &tir::OpHandle) -> bool {
    speculatable_if_ignore(
        super::semantics::operation_semantics(operation)
            .arithmetic()
            .map(|s| s.exceptions),
    )
}

fn arithmetic_conversion_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    let semantics = super::semantics::operation_semantics(operation);
    let semantics = semantics.arithmetic().expect("verified semantics");
    effect_records(
        operation,
        effects_for(Some(semantics.rounding), semantics.exceptions),
    )
}

fn arithmetic_conversion_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
) -> ResourceSemantics {
    let semantics = super::semantics::operation_semantics(operation);
    let result_type = operation
        .context
        .get_value(operation.value_results()[0])
        .ty();
    let width =
        float_width(&operation.context, result_type).expect("verified floating conversion result");
    let format = float_parts(width);
    super::resource::arithmetic(
        operation,
        generic,
        rounded,
        Some(format),
        semantics
            .arithmetic()
            .expect("verified arithmetic conversion semantics"),
    )
}

fn arithmetic_conversion_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    state: &mut crate::interp::ExecutionState,
    kind: FloatOp,
    source_width: fn(&Context, TypeId) -> Result<FloatWidth, Error>,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let semantics = super::semantics::operation_semantics(operation);
    let semantics = semantics.arithmetic().map_err(interp_error)?;
    let source_type = operation
        .context
        .get_value(operation.value_operands()[0])
        .ty();
    let destination_type = operation
        .context
        .get_value(operation.value_results()[0])
        .ty();
    let source = source_width(&operation.context, source_type).map_err(interp_error)?;
    let destination = float_width(&operation.context, destination_type).map_err(interp_error)?;
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
            kind,
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

fn to_integer_value_semantics(
    operation: &tir::OpHandle,
    graph: &mut tir::sem::SemGraph,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
) -> Option<tir::NodeId> {
    let semantics = super::semantics::operation_semantics(operation);
    integer_conversion_semantics(
        operation,
        graph,
        generic,
        rounded,
        semantics.integer_conversion().ok()?,
    )
}

fn to_integer_verifiable_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    float_width(
        context,
        context.get_value(operation.value_operands()[0]).ty(),
    )?;
    int_width(context, context.get_value(operation.results()[0]).ty())?;
    let semantics = super::semantics::operation_semantics(operation);
    let semantics = semantics.integer_conversion()?;
    if semantics.subnormals != SubnormalMode::Gradual {
        return Err(Error::VerificationError(
            "FP conversion supports gradual subnormals".into(),
        ));
    }
    verify_ports(
        operation,
        &effects_for(Some(semantics.rounding), semantics.exceptions),
    )
}

fn to_integer_speculatable_is_speculatable(operation: &tir::OpHandle) -> bool {
    speculatable_if_ignore(
        super::semantics::operation_semantics(operation)
            .integer_conversion()
            .map(|s| s.exceptions),
    )
}

fn to_integer_resourceeffects_resource_effects(operation: &tir::OpHandle) -> Vec<ResourceEffect> {
    let semantics = super::semantics::operation_semantics(operation);
    let semantics = semantics.integer_conversion().expect("verified semantics");
    effect_records(
        operation,
        effects_for(Some(semantics.rounding), semantics.exceptions),
    )
}

fn to_integer_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
) -> ResourceSemantics {
    let semantics = super::semantics::operation_semantics(operation);
    super::resource::integer_conversion(
        operation,
        generic,
        rounded,
        semantics
            .integer_conversion()
            .expect("verified integer conversion semantics"),
    )
}

fn to_integer_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    state: &mut crate::interp::ExecutionState,
    kind: FloatOp,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let crate::interp::Value::Float(value) = &operands[0] else {
        return Err(crate::interp::InterpError::Message(
            "FP-to-integer conversion requires a float".into(),
        ));
    };
    let semantics = super::semantics::operation_semantics(operation);
    let semantics = semantics.integer_conversion().map_err(interp_error)?;
    let source = width_of_float(value)?;
    let destination = int_width(
        &operation.context,
        operation.context.get_value(operation.results()[0]).ty(),
    )
    .map_err(interp_error)?;
    let result = apply_flags(
        eval_float(
            kind,
            source,
            destination,
            [value.to_bits() as u64, 0, 0],
            rounding(semantics.rounding, state),
        ),
        semantics.exceptions,
        state,
    )?;
    let width = u64::from(destination.bit_width());
    Ok(vec![crate::interp::Value::Int(APInt::new(
        width,
        result.bits,
    ))])
}
