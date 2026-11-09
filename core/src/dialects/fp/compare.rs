use tir_adt::{APInt, ComparisonKind, compare_float};

use super::arithmetic::{float_width, width_of_float};
use super::resource::{apply_flags, effect_records, effects_for, verify_ports};
use super::{ComparisonBehavior, Exceptions, SubnormalMode};
use crate::NodeId;
use crate::builtin::{IntegerType, StateResource};
use crate::sem::{SemGraph, SymKind, SymPayload};
use crate::{
    Context, Error, HasResourceSemantics, ResourceEffect, ResourceEffects, ResourceSemantics,
};

use super::semantics::{interp_error, speculatable_if_ignore};
use crate as tir;

tir::helpers::op_def! {
    file: "defs/converted/dialects_fp_compare.cdsl",
    hosts: {
        cmp_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("cmp_expr")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "cmp_expr" => (crate::fp::cmp_cmp_expr)(operation, graph),
                    _ => None,
                },
            )
        },
        cmp_verifier => crate::fp::cmp_verify_impl,
        cmp_iface_ResourceEffects => (ResourceEffects, &crate::fp::__TIR_IFACE_CmpOp_ResourceEffects),
        cmp_iface_HasResourceSemantics => (HasResourceSemantics, &crate::fp::__TIR_IFACE_CmpOp_HasResourceSemantics),
        cmp_iface_Speculatable => (crate :: Speculatable, &crate::fp::__TIR_IFACE_CmpOp_Speculatable),
        cmp_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_CmpOp_Interp),
    },
}

pub(crate) fn cmp_cmp_expr(
    operation: &tir::OpHandle,
    graph: &mut tir::sem::SemGraph,
) -> Option<tir::NodeId> {
    tir_symbolic::sem::cmpf_semantics(graph, crate::fp::cmp_predicate(operation))
}

pub(crate) fn cmp_verify_impl(operation: &tir::OpHandle, context: &Context) -> Result<(), Error> {
    let operands = operation.value_operands();
    let lhs = context.get_value(operands[0]).ty();
    if lhs != context.get_value(operands[1]).ty() {
        return Err(Error::VerificationError(
            "fp comparison operands must have one format".into(),
        ));
    }
    float_width(context, lhs)?;
    let semantics = crate::fp::cmp_semantics(operation);
    let semantics = semantics.comparison()?;
    if semantics.subnormals != SubnormalMode::Gradual {
        return Err(Error::VerificationError(
            "fp comparison supports gradual subnormals".into(),
        ));
    }
    let required = effects_for(None, semantics.exceptions);
    verify_ports(operation, &required)
}

pub(crate) fn cmp_speculatable_is_speculatable(operation: &tir::OpHandle) -> bool {
    speculatable_if_ignore(
        crate::fp::cmp_semantics(operation)
            .comparison()
            .map(|s| s.exceptions),
    )
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CmpOp_Speculatable: crate::SpeculatableCallbacks =
    crate::SpeculatableCallbacks {
        is_speculatable: Some(crate::fp::cmp_speculatable_is_speculatable),
    };

pub(crate) fn cmp_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    let semantics = crate::fp::cmp_semantics(operation);
    let semantics = semantics
        .comparison()
        .expect("verified comparison semantics");
    effect_records(operation, effects_for(None, semantics.exceptions))
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CmpOp_ResourceEffects: crate::ResourceEffectsCallbacks =
    crate::ResourceEffectsCallbacks {
        resource_effects: crate::fp::cmp_resourceeffects_resource_effects,
    };

fn comparison_result(op: &crate::OpHandle, graph: &mut SemGraph) -> NodeId {
    let mut expression = SemGraph::new();
    let root = tir_symbolic::sem::cmpf_semantics(&mut expression, crate::fp::cmp_predicate(op))
        .expect("verified floating predicate has symbolic semantics");
    let operands = op.value_operands();
    let mut memo = std::collections::HashMap::new();
    for node in expression.preorder(root) {
        let Some(SymPayload::SymbolId(symbol)) = expression.get_leaf_data(node) else {
            continue;
        };
        memo.insert(
            node.index(),
            super::resource::value(graph, op, operands[*symbol as usize]),
        );
    }
    let result = tir_symbolic::sem::copy_subgraph(graph, &expression, root, &mut memo);
    graph.annotation_mut(result).actual_type = Some(IntegerType::new(&op.context, 1));
    result
}

fn nan_test(
    graph: &mut SemGraph,
    context: &Context,
    input: NodeId,
    width: u32,
    signaling_only: bool,
) -> NodeId {
    let bits = super::resource::operation(graph, SymKind::Bitcast, &[input]);
    graph.annotation_mut(bits).actual_type = Some(IntegerType::new(context, u64::from(width)));
    let (signless, infinity, quiet) = match width {
        32 => (0x7fff_ffff, 0x7f80_0000, 0x0040_0000),
        64 => (
            0x7fff_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0x0008_0000_0000_0000,
        ),
        _ => unreachable!("verified comparison format"),
    };
    let mask = super::resource::constant(graph, width, signless);
    let magnitude = super::resource::operation(graph, SymKind::And, &[bits, mask]);
    let infinity = super::resource::constant(graph, width, infinity);
    let nan = super::resource::operation(graph, SymKind::UGt, &[magnitude, infinity]);
    graph.annotation_mut(nan).actual_type = Some(IntegerType::new(context, 1));
    if !signaling_only {
        return nan;
    }
    let quiet = super::resource::constant(graph, width, quiet);
    let quiet = super::resource::operation(graph, SymKind::And, &[bits, quiet]);
    let zero = super::resource::constant(graph, width, 0);
    let signaling = super::resource::operation(graph, SymKind::Eq, &[quiet, zero]);
    graph.annotation_mut(signaling).actual_type = Some(IntegerType::new(context, 1));
    let result = super::resource::operation(graph, SymKind::And, &[nan, signaling]);
    graph.annotation_mut(result).actual_type = Some(IntegerType::new(context, 1));
    result
}

pub(crate) fn cmp_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    let semantics = crate::fp::cmp_semantics(operation);
    let semantics = semantics
        .comparison()
        .expect("verified comparison semantics");
    let mut graph = SemGraph::new();
    let result = comparison_result(operation, &mut graph);
    if semantics.exceptions == Exceptions::Ignore {
        return ResourceSemantics {
            graph,
            raised_flags: None,
            root: result,
            value_results: vec![result],
            state_results: Vec::new(),
        };
    }
    let operands = operation.value_operands();
    let width = float_width(
        &operation.context,
        operation.context.get_value(operands[0]).ty(),
    )
    .expect("verified comparison format")
    .bit_width();
    let signaling_only = semantics.behavior == ComparisonBehavior::Quiet;
    let lhs = super::resource::value(&mut graph, operation, operands[0]);
    let rhs = super::resource::value(&mut graph, operation, operands[1]);
    let lhs_invalid = nan_test(&mut graph, &operation.context, lhs, width, signaling_only);
    let rhs_invalid = nan_test(&mut graph, &operation.context, rhs, width, signaling_only);
    let invalid = super::resource::operation(&mut graph, SymKind::Or, &[lhs_invalid, rhs_invalid]);
    graph.annotation_mut(invalid).actual_type = Some(IntegerType::new(&operation.context, 1));
    let invalid_flag = super::resource::constant(&mut graph, 5, 0b1_0000);
    let no_flags = super::resource::constant(&mut graph, 5, 0);
    let raised =
        super::resource::operation(&mut graph, SymKind::If, &[invalid, invalid_flag, no_flags]);
    graph.annotation_mut(raised).actual_type = Some(IntegerType::new(&operation.context, 5));
    let environment = super::resource::value(
        &mut graph,
        operation,
        super::resource::state_for(operation, StateResource::FpEnv),
    );
    super::resource::flagged_result(
        operation,
        graph,
        result,
        raised,
        semantics.exceptions,
        environment,
    )
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CmpOp_HasResourceSemantics: crate::HasResourceSemanticsCallbacks =
    crate::HasResourceSemanticsCallbacks {
        resource_semantics: crate::fp::cmp_hasresourcesemantics_resource_semantics,
    };

pub(crate) fn cmp_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    state: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let [
        crate::interp::Value::Float(lhs),
        crate::interp::Value::Float(rhs),
    ] = operands
    else {
        return Err(crate::interp::InterpError::Message(
            "fp comparison requires floating operands".into(),
        ));
    };
    let width = width_of_float(lhs)?;
    if width_of_float(rhs)? != width {
        return Err(crate::interp::InterpError::Message(
            "fp comparison operands have different formats".into(),
        ));
    }
    let semantics = crate::fp::cmp_semantics(operation);
    let semantics = semantics.comparison().map_err(interp_error)?;
    let result = compare_float(
        width,
        lhs.to_bits() as u64,
        rhs.to_bits() as u64,
        crate::fp::cmp_predicate(operation),
        match semantics.behavior {
            ComparisonBehavior::Quiet => ComparisonKind::Quiet,
            ComparisonBehavior::Signaling => ComparisonKind::Signaling,
        },
    );
    let result = apply_flags(result, semantics.exceptions, state)?;
    Ok(vec![crate::interp::Value::Int(APInt::new(1, result.bits))])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CmpOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::cmp_interp_evaluate,
    };
