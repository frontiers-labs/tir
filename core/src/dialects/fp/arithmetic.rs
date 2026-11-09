use tir_adt::{APFloat, FloatOp, FloatWidth, eval_float};

use super::resource::{apply_flags, effect_records, effects_for, verify_ports};
use super::semantics::{interp_error, rounding_code, speculatable_if_ignore};
use super::{ArithmeticSemantics, NaNPolicy, Rounding, SubnormalMode, Tininess};
use crate::builtin::FloatType;
use crate::{
    Context, Error, HasResourceSemantics, ResourceEffect, ResourceEffects, ResourceSemantics,
    TypeId,
};

use crate as tir;

fn verify(
    context: &Context,
    op: &tir::OpHandle,
    semantics: &ArithmeticSemantics,
) -> Result<(), Error> {
    let operands = op.value_operands();
    let result_type = context.get_value(op.value_results()[0]).ty();
    if operands
        .iter()
        .any(|value| context.get_value(*value).ty() != result_type)
    {
        return Err(Error::VerificationError(
            "fp arithmetic operands and result must have one format".into(),
        ));
    }
    float_width(context, result_type)?;
    verify_policy(semantics)?;
    let required = effects_for(Some(semantics.rounding), semantics.exceptions);
    verify_ports(op, &required)
}

pub(super) fn verify_policy(semantics: &ArithmeticSemantics) -> Result<(), Error> {
    if semantics.subnormals != SubnormalMode::Gradual
        || semantics.tininess != Tininess::AfterRounding
    {
        return Err(Error::VerificationError(
            "fp arithmetic supports gradual subnormals and tininess after rounding".into(),
        ));
    }
    Ok(())
}

fn evaluate(
    op: FloatOp,
    semantics: &ArithmeticSemantics,
    operands: &[crate::interp::Value],
    state: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let mut bits = [0; 3];
    let mut width = None;
    for (index, operand) in operands.iter().enumerate() {
        let crate::interp::Value::Float(value) = operand else {
            return Err(crate::interp::InterpError::Message(
                "fp arithmetic requires floating operands".into(),
            ));
        };
        let operand_width = width_of_float(value)?;
        if width.is_some_and(|width| width != operand_width) {
            return Err(crate::interp::InterpError::Message(
                "fp arithmetic operands have different formats".into(),
            ));
        }
        width = Some(operand_width);
        bits[index] = value.to_bits() as u64;
    }
    let width = width.expect("arithmetic operation has an operand");
    let rounding = match semantics.rounding {
        Rounding::Fixed(rounding) => rounding,
        Rounding::Dynamic => state.fp_environment.rounding,
    };
    let evaluated = apply_flags(
        eval_float(op, width, width, bits, rounding),
        semantics.exceptions,
        state,
    )?;
    let result = match semantics.nan {
        NaNPolicy::Canonical => canonicalize_nan(width, evaluated.bits),
        _ => evaluated.bits,
    };
    let (exponent, mantissa) = float_parts(width);
    Ok(vec![crate::interp::Value::Float(APFloat::from_bits(
        exponent,
        mantissa,
        false,
        result as u128,
    ))])
}

pub(crate) fn rounding_node(
    graph: &mut tir::sem::SemGraph,
    rounding: Rounding,
) -> Option<tir::NodeId> {
    let Rounding::Fixed(rounding) = rounding else {
        return None;
    };
    let value = rounding_code(rounding);
    let node = graph.add_node(tir::sem::SymKind::Constant);
    graph.set_leaf_data(
        node,
        tir::sem::SymPayload::Int(tir_adt::APInt::new(3, value)),
    );
    Some(node)
}

tir::helpers::op_def! {
    file: "defs/converted/dialects_fp_arithmetic.cdsl",
    hosts: {
        add_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_op_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::FAdd,
                        tir::sem::SymKind::FAddRound,
                        2,
                    ),
                    _ => None,
                },
            )
        },
        add_verifier => arithmetic_op_verifiable_verify_impl,
        add_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_op_resourceeffects_resource_effects(operation) }),
        add_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_op_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::FAdd , tir::sem::SymKind::FAddRound ) }),
        add_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_op_speculatable_is_speculatable) }),
        add_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_op_interp_evaluate(operation, operands, state, FloatOp::Add ) }),
        sub_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_op_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::FSub,
                        tir::sem::SymKind::FSubRound,
                        2,
                    ),
                    _ => None,
                },
            )
        },
        sub_verifier => arithmetic_op_verifiable_verify_impl,
        sub_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_op_resourceeffects_resource_effects(operation) }),
        sub_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_op_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::FSub , tir::sem::SymKind::FSubRound ) }),
        sub_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_op_speculatable_is_speculatable) }),
        sub_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_op_interp_evaluate(operation, operands, state, FloatOp::Sub ) }),
        mul_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_op_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::FMul,
                        tir::sem::SymKind::FMulRound,
                        2,
                    ),
                    _ => None,
                },
            )
        },
        mul_verifier => arithmetic_op_verifiable_verify_impl,
        mul_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_op_resourceeffects_resource_effects(operation) }),
        mul_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_op_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::FMul , tir::sem::SymKind::FMulRound ) }),
        mul_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_op_speculatable_is_speculatable) }),
        mul_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_op_interp_evaluate(operation, operands, state, FloatOp::Mul ) }),
        div_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_op_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::FDiv,
                        tir::sem::SymKind::FDivRound,
                        2,
                    ),
                    _ => None,
                },
            )
        },
        div_verifier => arithmetic_op_verifiable_verify_impl,
        div_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_op_resourceeffects_resource_effects(operation) }),
        div_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_op_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::FDiv , tir::sem::SymKind::FDivRound ) }),
        div_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_op_speculatable_is_speculatable) }),
        div_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_op_interp_evaluate(operation, operands, state, FloatOp::Div ) }),
        fma_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_op_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::Fma,
                        tir::sem::SymKind::FmaRound,
                        3,
                    ),
                    _ => None,
                },
            )
        },
        fma_verifier => arithmetic_op_verifiable_verify_impl,
        fma_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_op_resourceeffects_resource_effects(operation) }),
        fma_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_op_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::Fma , tir::sem::SymKind::FmaRound ) }),
        fma_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_op_speculatable_is_speculatable) }),
        fma_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_op_interp_evaluate(operation, operands, state, FloatOp::Fma ) }),
        sqrt_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => arithmetic_op_value_semantics(
                        operation,
                        graph,
                        tir::sem::SymKind::Sqrt,
                        tir::sem::SymKind::SqrtRound,
                        1,
                    ),
                    _ => None,
                },
            )
        },
        sqrt_verifier => arithmetic_op_verifiable_verify_impl,
        sqrt_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| arithmetic_op_resourceeffects_resource_effects(operation) }),
        sqrt_iface_HasResourceSemantics => (HasResourceSemantics, &crate::HasResourceSemanticsCallbacks { resource_semantics: |operation| arithmetic_op_hasresourcesemantics_resource_semantics(operation, tir::sem::SymKind::Sqrt , tir::sem::SymKind::SqrtRound ) }),
        sqrt_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks { is_speculatable: Some(arithmetic_op_speculatable_is_speculatable) }),
        sqrt_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |operation, operands, state| arithmetic_op_interp_evaluate(operation, operands, state, FloatOp::Sqrt ) }),
    },
}

fn arithmetic_semantics(
    graph: &mut tir::sem::SemGraph,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
    arity: usize,
    width: FloatWidth,
    semantics: &ArithmeticSemantics,
) -> Option<tir::NodeId> {
    let operands: Vec<_> = (0..arity)
        .map(|index| {
            let node = graph.add_node(tir::sem::SymKind::Symbol);
            graph.set_leaf_data(node, tir::sem::SymPayload::SymbolId(index as u32));
            node
        })
        .collect();
    let nearest_any_quiet = semantics.nan == NaNPolicy::AnyQuiet
        && semantics.rounding == Rounding::Fixed(tir_adt::RoundingMode::TiesToEven);
    let rounding = if nearest_any_quiet {
        None
    } else {
        Some(rounding_node(graph, semantics.rounding)?)
    };
    let canonical_bits = (semantics.nan == NaNPolicy::Canonical)
        .then(|| super::resource::constant(graph, width.bit_width(), canonical_nan(width)));
    Some(
        build_arithmetic_value(
            graph,
            &operands,
            (generic, rounded),
            nearest_any_quiet,
            rounding,
            semantics.nan,
            canonical_bits,
        )
        .0,
    )
}

pub(super) fn build_arithmetic_value(
    graph: &mut tir::sem::SemGraph,
    operands: &[tir::NodeId],
    kinds: (tir::sem::SymKind, tir::sem::SymKind),
    use_generic: bool,
    rounding: Option<tir::NodeId>,
    nan: NaNPolicy,
    canonical_bits: Option<tir::NodeId>,
) -> (tir::NodeId, tir::NodeId) {
    let evaluation = value_operation(
        graph,
        if use_generic { kinds.0 } else { kinds.1 },
        operands,
        rounding,
    );
    let value = match nan {
        NaNPolicy::PreservePayload => preserve_payload_observation(graph, evaluation),
        NaNPolicy::Canonical => super::resource::canonicalize_nan(
            graph,
            evaluation,
            canonical_bits.expect("canonical NaN policy has canonical bits"),
        ),
        NaNPolicy::AnyQuiet => evaluation,
    };
    (value, evaluation)
}

pub(super) fn value_operation(
    graph: &mut tir::sem::SemGraph,
    kind: tir::sem::SymKind,
    operands: &[tir::NodeId],
    rounding: Option<tir::NodeId>,
) -> tir::NodeId {
    let operation = graph.add_node(kind);
    for &operand in operands {
        graph.add_edge(operation, operand);
    }
    let Some(rounding) = rounding else {
        return operation;
    };
    graph.add_edge(operation, rounding);
    operation
}

pub(crate) fn preserve_payload_observation(
    graph: &mut tir::sem::SemGraph,
    value: tir::NodeId,
) -> tir::NodeId {
    let bits = graph.add_node(tir::sem::SymKind::Bitcast);
    graph.add_edge(bits, value);
    let observed = graph.add_node(tir::sem::SymKind::AsFloat);
    graph.add_edge(observed, bits);
    observed
}

pub(crate) fn float_width(context: &Context, ty: TypeId) -> Result<FloatWidth, Error> {
    match float_type_parts(context, ty)? {
        (8, 23) => Ok(FloatWidth::W32),
        (11, 52) => Ok(FloatWidth::W64),
        _ => Err(Error::VerificationError(
            "fp arithmetic supports only binary32 and binary64".into(),
        )),
    }
}

pub(super) fn float_type_parts(context: &Context, ty: TypeId) -> Result<(u32, u32), Error> {
    let float = FloatType::view(context, ty)
        .ok_or_else(|| Error::VerificationError("expected floating type".into()))?;
    Ok((float.exp_width(), float.mant_width()))
}

pub(crate) fn width_of_float(value: &APFloat) -> Result<FloatWidth, crate::interp::InterpError> {
    match (value.exp_width(), value.mant_width()) {
        (8, 23) => Ok(FloatWidth::W32),
        (11, 52) => Ok(FloatWidth::W64),
        _ => Err(crate::interp::InterpError::Message(
            "fp arithmetic supports only binary32 and binary64".into(),
        )),
    }
}

pub(crate) fn float_parts(width: FloatWidth) -> (u32, u32) {
    match width {
        FloatWidth::W32 => (8, 23),
        FloatWidth::W64 => (11, 52),
    }
}

fn is_nan(width: FloatWidth, bits: u64) -> bool {
    match width {
        FloatWidth::W32 => bits & 0x7f80_0000 == 0x7f80_0000 && bits & 0x007f_ffff != 0,
        FloatWidth::W64 => {
            bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000
                && bits & 0x000f_ffff_ffff_ffff != 0
        }
    }
}

pub(super) fn canonical_nan(width: FloatWidth) -> u64 {
    match width {
        FloatWidth::W32 => 0x7fc0_0000,
        FloatWidth::W64 => 0x7ff8_0000_0000_0000,
    }
}

pub(super) fn canonicalize_nan(width: FloatWidth, bits: u64) -> u64 {
    if is_nan(width, bits) {
        canonical_nan(width)
    } else {
        bits
    }
}

fn arithmetic_op_value_semantics(
    operation: &tir::OpHandle,
    graph: &mut tir::sem::SemGraph,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
    arity: usize,
) -> Option<tir::NodeId> {
    let semantics = super::semantics::operation_semantics(operation);
    arithmetic_semantics(
        graph,
        generic,
        rounded,
        arity,
        float_width(
            &operation.context,
            operation
                .context
                .get_value(operation.value_results()[0])
                .ty(),
        )
        .ok()?,
        semantics.arithmetic().ok()?,
    )
}

fn arithmetic_op_verifiable_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    let semantics = super::semantics::operation_semantics(operation);
    verify(context, operation, semantics.arithmetic()?)
}

fn arithmetic_op_speculatable_is_speculatable(operation: &tir::OpHandle) -> bool {
    speculatable_if_ignore(
        super::semantics::operation_semantics(operation)
            .arithmetic()
            .map(|s| s.exceptions),
    )
}

fn arithmetic_op_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    let semantics = super::semantics::operation_semantics(operation);
    let semantics = semantics
        .arithmetic()
        .expect("verified arithmetic semantics");
    effect_records(
        operation,
        effects_for(Some(semantics.rounding), semantics.exceptions),
    )
}

fn arithmetic_op_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
    generic: tir::sem::SymKind,
    rounded: tir::sem::SymKind,
) -> ResourceSemantics {
    let semantics = super::semantics::operation_semantics(operation);
    super::resource::arithmetic(
        operation,
        generic,
        rounded,
        None,
        semantics
            .arithmetic()
            .expect("verified arithmetic semantics"),
    )
}

fn arithmetic_op_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    state: &mut crate::interp::ExecutionState,
    kind: FloatOp,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let semantics = super::semantics::operation_semantics(operation);
    let semantics = semantics.arithmetic().map_err(interp_error)?;
    evaluate(kind, semantics, operands, state)
}
