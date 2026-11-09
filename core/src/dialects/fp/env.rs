use tir_adt::{APInt, RoundingMode};

use super::resource::{effect_records, state_for, transition_semantics, verify_ports};
use super::semantics::{parse_rounding, rounding_code};
use super::{EnvironmentType, RoundingType, resource};
use crate::NodeId;
use crate::builtin::StateResource;
use crate::sem::{SemGraph, SymKind};
use crate::{
    Context, Error, HasResourceSemantics, ResourceAccess, ResourceEffect, ResourceEffects,
    ResourceField, ResourceSemantics, ValueId,
};

use crate as tir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FloatEnvironment {
    pub rounding: RoundingMode,
    pub flags: u8,
    pub traps: u8,
}

impl Default for FloatEnvironment {
    fn default() -> Self {
        Self {
            rounding: RoundingMode::TiesToEven,
            flags: 0,
            traps: 0,
        }
    }
}

tir::helpers::op_def! {
    file: "defs/converted/dialects_fp_env.cdsl",
    hosts: {
        rounding_constant_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("value_semantics")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "value_semantics" => (crate::fp::rounding_constant_value_semantics)(operation, graph),
                    _ => None,
                },
            )
        },
        rounding_constant_verifier => crate::fp::rounding_constant_verify_impl,
        rounding_constant_iface_ConstantLike => (crate :: ConstantLike, &crate::fp::__TIR_IFACE_RoundingConstantOp_ConstantLike),
        rounding_constant_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_RoundingConstantOp_Interp),
        rounding_constant_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        get_round_verifier => |operation, _context| env_verify(operation, ResourceAccess::Read ),
        get_round_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Read ), }),
        get_round_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_GetRoundOp_HasResourceSemantics),
        get_round_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, _operands , state | { Ok(vec![crate::interp::Value::Rounding(state.fp_environment.rounding)]) } , }),
        set_round_verifier => |operation, _context| env_verify(operation, ResourceAccess::Change ),
        set_round_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Change ), }),
        set_round_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_SetRoundOp_HasResourceSemantics),
        set_round_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, operands , state | { let [crate::interp::Value::Rounding(mode)] = operands else { return Err(crate::interp::InterpError::Message( "fp.set_round requires !fp.rounding".into(), )); }; state.fp_environment.rounding = *mode; Ok(Vec::new()) } , }),
        get_flags_verifier => |operation, _context| env_verify(operation, ResourceAccess::Read ),
        get_flags_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Read ), }),
        get_flags_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_GetFlagsOp_HasResourceSemantics),
        get_flags_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, _operands , state | { Ok(vec![crate::interp::Value::Int(APInt::new( 5, state.fp_environment.flags as u64, ))]) } , }),
        clear_flags_verifier => |operation, _context| env_verify(operation, ResourceAccess::Change ),
        clear_flags_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Change ), }),
        clear_flags_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_ClearFlagsOp_HasResourceSemantics),
        clear_flags_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, operands , state | { state.fp_environment.flags &= !mask(&operands[0])?; Ok(Vec::new()) } , }),
        raise_flags_verifier => trapping_verify,
        raise_flags_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: trapping_resource_effects }),
        raise_flags_iface_HasResourceSemantics => (HasResourceSemantics, &crate::fp::__TIR_IFACE_RaiseFlagsOp_HasResourceSemantics),
        raise_flags_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_RaiseFlagsOp_Interp),
        get_traps_verifier => |operation, _context| env_verify(operation, ResourceAccess::Read ),
        get_traps_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Read ), }),
        get_traps_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_GetTrapsOp_HasResourceSemantics),
        get_traps_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, _operands , state | { Ok(vec![crate::interp::Value::Int(APInt::new( 5, state.fp_environment.traps as u64, ))]) } , }),
        set_traps_verifier => |operation, _context| env_verify(operation, ResourceAccess::Change ),
        set_traps_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Change ), }),
        set_traps_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_SetTrapsOp_HasResourceSemantics),
        set_traps_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, operands , state | { state.fp_environment.traps = mask(&operands[0])?; Ok(Vec::new()) } , }),
        save_verifier => |operation, _context| env_verify(operation, ResourceAccess::Read ),
        save_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Read ), }),
        save_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_SaveOp_HasResourceSemantics),
        save_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, _operands , state | { Ok(vec![crate::interp::Value::FpEnvironment(state.fp_environment)]) } , }),
        restore_verifier => |operation, _context| env_verify(operation, ResourceAccess::Change ),
        restore_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Change ), }),
        restore_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_RestoreOp_HasResourceSemantics),
        restore_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, operands , state | { let [crate::interp::Value::FpEnvironment(snapshot)] = operands else { return Err(crate::interp::InterpError::Message( "fp.restore requires !fp.env".into(), )); }; state.fp_environment = *snapshot; Ok(Vec::new()) } , }),
        hold_verifier => |operation, _context| env_verify(operation, ResourceAccess::Change ),
        hold_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: |operation| env_resource_effects(operation, ResourceAccess::Change ), }),
        hold_iface_HasResourceSemantics => (HasResourceSemantics, &__TIR_IFACE_HoldOp_HasResourceSemantics),
        hold_iface_Interp => (crate :: interp :: Interp, &crate::interp::InterpCallbacks { evaluate: |_operation, _operands , state | { let snapshot = state.fp_environment; state.fp_environment.flags = 0; state.fp_environment.traps = 0; Ok(vec![crate::interp::Value::FpEnvironment(snapshot)]) } , }),
        update_verifier => trapping_verify,
        update_iface_ResourceEffects => (ResourceEffects, &crate::ResourceEffectsCallbacks { resource_effects: trapping_resource_effects }),
        update_iface_HasResourceSemantics => (HasResourceSemantics, &crate::fp::__TIR_IFACE_UpdateOp_HasResourceSemantics),
        update_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_UpdateOp_Interp),
    },
}

/// The mode as the environment's rounding field holds it.
pub fn rounding_constant_code(operation: &tir::OpHandle) -> Option<u64> {
    parse_rounding(&crate::fp::rounding_constant_mode(operation)).map(rounding_code)
}

pub(crate) fn rounding_constant_value_semantics(
    operation: &tir::OpHandle,
    graph: &mut crate::sem::SemGraph,
) -> Option<NodeId> {
    Some(resource::constant(
        graph,
        3,
        crate::fp::rounding_constant_code(operation)?,
    ))
}

fn env_verify(operation: &tir::OpHandle, access: ResourceAccess) -> Result<(), Error> {
    verify_ports(operation, &required_effects(access, false))
}

fn env_resource_effects(operation: &tir::OpHandle, access: ResourceAccess) -> Vec<ResourceEffect> {
    effect_records(operation, required_effects(access, false))
}

pub(crate) fn rounding_constant_constantlike_constant_value(operation: &tir::OpHandle) -> APInt {
    APInt::new(
        3,
        crate::fp::rounding_constant_code(operation).unwrap_or_default(),
    )
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_RoundingConstantOp_ConstantLike: crate::ConstantLikeCallbacks =
    crate::ConstantLikeCallbacks {
        constant_value: crate::fp::rounding_constant_constantlike_constant_value,
    };

fn required_effects(access: ResourceAccess, memory: bool) -> Vec<(StateResource, ResourceAccess)> {
    let mut effects = vec![(StateResource::FpEnv, access)];
    effects.extend(memory.then_some((StateResource::Memory, ResourceAccess::Change)));
    effects
}

fn read_field_semantics(op: &tir::OpHandle, field_kind: ResourceField) -> ResourceSemantics {
    let context = &op.context;
    let mut graph = SemGraph::new();
    let state = resource::value(&mut graph, op, op.state_operands()[0]);
    let resource = resource::resource(&mut graph, StateResource::FpEnv);
    let field = resource::field(&mut graph, field_kind);
    let value_ty = match field_kind {
        ResourceField::FpRounding => crate::builtin::IntegerType::new(context, 3),
        ResourceField::FpFlags | ResourceField::FpTraps => {
            crate::builtin::IntegerType::new(context, 5)
        }
        ResourceField::Whole => unreachable!("FP snapshots compose individual fields"),
    };
    let value = resource::state_read(&mut graph, state, resource, field, value_ty);
    let observation = resource::state_assign(
        &mut graph,
        state,
        resource,
        field,
        ResourceAccess::Read,
        value,
        context.get_value(op.state_results()[0]).ty(),
    );
    ResourceSemantics {
        graph,
        raised_flags: None,
        root: value,
        value_results: vec![value],
        state_results: vec![observation],
    }
}

fn write_field_semantics(
    op: &tir::OpHandle,
    value: ValueId,
    field_kind: ResourceField,
) -> ResourceSemantics {
    let context = &op.context;
    let mut graph = SemGraph::new();
    let state = resource::value(&mut graph, op, op.state_operands()[0]);
    let resource = resource::resource(&mut graph, StateResource::FpEnv);
    let field = resource::field(&mut graph, field_kind);
    let value = resource::value(&mut graph, op, value);
    let root = resource::state_assign(
        &mut graph,
        state,
        resource,
        field,
        ResourceAccess::Change,
        value,
        context.get_value(op.state_results()[0]).ty(),
    );
    ResourceSemantics {
        graph,
        raised_flags: None,
        root,
        value_results: Vec::new(),
        state_results: vec![root],
    }
}

pub(crate) fn get_round_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    read_field_semantics(operation, ResourceField::FpRounding)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_GetRoundOp_HasResourceSemantics:
    crate::HasResourceSemanticsCallbacks = crate::HasResourceSemanticsCallbacks {
    resource_semantics: crate::fp::get_round_hasresourcesemantics_resource_semantics,
};

pub(crate) fn set_round_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    write_field_semantics(
        operation,
        operation.operands()[0],
        ResourceField::FpRounding,
    )
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SetRoundOp_HasResourceSemantics:
    crate::HasResourceSemanticsCallbacks = crate::HasResourceSemanticsCallbacks {
    resource_semantics: crate::fp::set_round_hasresourcesemantics_resource_semantics,
};

pub(crate) fn clear_flags_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    let context = &operation.context;
    let mut graph = SemGraph::new();
    let state = resource::value(&mut graph, operation, operation.state_operands()[0]);
    let resource = resource::resource(&mut graph, StateResource::FpEnv);
    let field = resource::field(&mut graph, ResourceField::FpFlags);
    let old = resource::state_read(
        &mut graph,
        state,
        resource,
        field,
        crate::builtin::IntegerType::new(context, 5),
    );
    let mask = resource::value(&mut graph, operation, operation.operands()[0]);
    let ones = resource::constant(&mut graph, 5, 0b1_1111);
    let inverse = resource::operation(&mut graph, SymKind::Xor, &[mask, ones]);
    let flags = resource::operation(&mut graph, SymKind::And, &[old, inverse]);
    let root = resource::state_assign(
        &mut graph,
        state,
        resource,
        field,
        ResourceAccess::Change,
        flags,
        context.get_value(operation.state_results()[0]).ty(),
    );
    ResourceSemantics {
        graph,
        raised_flags: None,
        root,
        value_results: Vec::new(),
        state_results: vec![root],
    }
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_ClearFlagsOp_HasResourceSemantics:
    crate::HasResourceSemanticsCallbacks = crate::HasResourceSemanticsCallbacks {
    resource_semantics: crate::fp::clear_flags_hasresourcesemantics_resource_semantics,
};

pub(crate) fn get_flags_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    read_field_semantics(operation, ResourceField::FpFlags)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_GetFlagsOp_HasResourceSemantics:
    crate::HasResourceSemanticsCallbacks = crate::HasResourceSemanticsCallbacks {
    resource_semantics: crate::fp::get_flags_hasresourcesemantics_resource_semantics,
};

pub(crate) fn get_traps_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    read_field_semantics(operation, ResourceField::FpTraps)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_GetTrapsOp_HasResourceSemantics:
    crate::HasResourceSemanticsCallbacks = crate::HasResourceSemanticsCallbacks {
    resource_semantics: crate::fp::get_traps_hasresourcesemantics_resource_semantics,
};

pub(crate) fn set_traps_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    write_field_semantics(operation, operation.operands()[0], ResourceField::FpTraps)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SetTrapsOp_HasResourceSemantics:
    crate::HasResourceSemanticsCallbacks = crate::HasResourceSemanticsCallbacks {
    resource_semantics: crate::fp::set_traps_hasresourcesemantics_resource_semantics,
};

pub(crate) fn save_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    let mut graph = SemGraph::new();
    let state = resource::value(&mut graph, operation, environment_state(operation));
    let snapshot = snapshot_value(&operation.context, &mut graph, state);
    transition_semantics(operation, Some(snapshot), state, None, graph)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SaveOp_HasResourceSemantics: crate::HasResourceSemanticsCallbacks =
    crate::HasResourceSemanticsCallbacks {
        resource_semantics: crate::fp::save_hasresourcesemantics_resource_semantics,
    };

pub(crate) fn restore_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    let mut graph = SemGraph::new();
    let state = environment_state(operation);
    let incoming = resource::value(&mut graph, operation, state);
    let snapshot = resource::value(&mut graph, operation, operation.value_operands()[0]);
    let restored = restore_snapshot(
        &mut graph,
        incoming,
        snapshot,
        operation.context.get_value(state).ty(),
    );
    transition_semantics(operation, None, restored, None, graph)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_RestoreOp_HasResourceSemantics: crate::HasResourceSemanticsCallbacks =
    crate::HasResourceSemanticsCallbacks {
        resource_semantics: crate::fp::restore_hasresourcesemantics_resource_semantics,
    };

fn environment_state(op: &tir::OpHandle) -> ValueId {
    state_for(op, StateResource::FpEnv)
}

fn memory_state(op: &tir::OpHandle) -> ValueId {
    state_for(op, StateResource::Memory)
}

fn assign_field(
    graph: &mut SemGraph,
    state: NodeId,
    field_kind: ResourceField,
    value: NodeId,
    state_ty: crate::TypeId,
) -> NodeId {
    let resource = resource::resource(graph, StateResource::FpEnv);
    let field = resource::field(graph, field_kind);
    resource::state_assign(
        graph,
        state,
        resource,
        field,
        ResourceAccess::Change,
        value,
        state_ty,
    )
}

fn read_field(
    graph: &mut SemGraph,
    state: NodeId,
    field_kind: ResourceField,
    ty: crate::TypeId,
) -> NodeId {
    let resource = resource::resource(graph, StateResource::FpEnv);
    let field = resource::field(graph, field_kind);
    resource::state_read(graph, state, resource, field, ty)
}

fn snapshot_value(context: &Context, graph: &mut SemGraph, state: NodeId) -> NodeId {
    let width = resource::constant(graph, 4, 13);
    let flags = read_field(
        graph,
        state,
        ResourceField::FpFlags,
        crate::builtin::IntegerType::new(context, 5),
    );
    let mut snapshot = resource::operation(graph, SymKind::ZExt, &[flags, width]);
    for (field, bits, shift) in [
        (ResourceField::FpRounding, 3, 5u64),
        (ResourceField::FpTraps, 5, 8),
    ] {
        let value = read_field(
            graph,
            state,
            field,
            crate::builtin::IntegerType::new(context, bits),
        );
        let value = resource::operation(graph, SymKind::ZExt, &[value, width]);
        let shift = resource::constant(graph, u64::BITS - shift.leading_zeros(), shift);
        let value = resource::operation(graph, SymKind::ShiftLeft, &[value, shift]);
        snapshot = resource::operation(graph, SymKind::Or, &[snapshot, value]);
    }
    graph.annotation_mut(snapshot).actual_type = Some(EnvironmentType::new(context));
    snapshot
}

fn restore_snapshot(
    graph: &mut SemGraph,
    mut state: NodeId,
    snapshot: NodeId,
    state_ty: crate::TypeId,
) -> NodeId {
    for (field, high, low) in [
        (ResourceField::FpFlags, 4u64, 0u64),
        (ResourceField::FpRounding, 7, 5),
        (ResourceField::FpTraps, 12, 8),
    ] {
        let high = resource::constant(graph, (u64::BITS - high.leading_zeros()).max(1), high);
        let low = resource::constant(graph, (u64::BITS - low.leading_zeros()).max(1), low);
        let value = resource::operation(graph, SymKind::Extract, &[snapshot, high, low]);
        state = assign_field(graph, state, field, value, state_ty);
    }
    state
}

fn trap_state(op: &tir::OpHandle, graph: &mut SemGraph, raised: NodeId, traps: NodeId) -> NodeId {
    let memory_value = memory_state(op);
    let memory = resource::value(graph, op, memory_value);
    let trapped = resource::operation(graph, SymKind::StateTrap, &[memory, raised, traps]);
    graph.annotation_mut(trapped).actual_type = Some(op.context.get_value(memory_value).ty());
    trapped
}

// Write `flags` to the environment and, when `trapping`, trap on the enabled
// bits of `raised`.
fn store_flags(
    op: &tir::OpHandle,
    mut graph: SemGraph,
    environment: NodeId,
    flags: NodeId,
    raised: NodeId,
    trapping: bool,
) -> ResourceSemantics {
    let environment_ty = op.context.get_value(environment_state(op)).ty();
    let environment = assign_field(
        &mut graph,
        environment,
        ResourceField::FpFlags,
        flags,
        environment_ty,
    );
    let memory = trapping.then(|| {
        let flags_ty = crate::builtin::IntegerType::new(&op.context, 5);
        let traps = read_field(&mut graph, environment, ResourceField::FpTraps, flags_ty);
        trap_state(op, &mut graph, raised, traps)
    });
    transition_semantics(op, None, environment, memory, graph)
}

pub(crate) fn raise_flags_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    let context = &operation.context;
    let mut graph = SemGraph::new();
    let environment_value = environment_state(operation);
    let environment = resource::value(&mut graph, operation, environment_value);
    let flags_ty = crate::builtin::IntegerType::new(context, 5);
    let old = read_field(&mut graph, environment, ResourceField::FpFlags, flags_ty);
    let raised = resource::value(&mut graph, operation, operation.value_operands()[0]);
    let flags = resource::operation(&mut graph, SymKind::Or, &[old, raised]);
    graph.annotation_mut(flags).actual_type = Some(flags_ty);
    store_flags(
        operation,
        graph,
        environment,
        flags,
        raised,
        crate::fp::raise_flags_trapping(operation),
    )
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_RaiseFlagsOp_HasResourceSemantics:
    crate::HasResourceSemanticsCallbacks = crate::HasResourceSemanticsCallbacks {
    resource_semantics: crate::fp::raise_flags_hasresourcesemantics_resource_semantics,
};

pub(crate) fn hold_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    let context = &operation.context;
    let mut graph = SemGraph::new();
    let environment_value = environment_state(operation);
    let environment = resource::value(&mut graph, operation, environment_value);
    let snapshot = snapshot_value(context, &mut graph, environment);
    let zero = resource::constant(&mut graph, 5, 0);
    let environment = assign_field(
        &mut graph,
        environment,
        ResourceField::FpFlags,
        zero,
        context.get_value(environment_value).ty(),
    );
    let environment = assign_field(
        &mut graph,
        environment,
        ResourceField::FpTraps,
        zero,
        context.get_value(environment_value).ty(),
    );
    transition_semantics(operation, Some(snapshot), environment, None, graph)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_HoldOp_HasResourceSemantics: crate::HasResourceSemanticsCallbacks =
    crate::HasResourceSemanticsCallbacks {
        resource_semantics: crate::fp::hold_hasresourcesemantics_resource_semantics,
    };

pub(crate) fn update_hasresourcesemantics_resource_semantics(
    operation: &tir::OpHandle,
) -> ResourceSemantics {
    let context = &operation.context;
    let mut graph = SemGraph::new();
    let environment_value = environment_state(operation);
    let environment = resource::value(&mut graph, operation, environment_value);
    let flags_ty = crate::builtin::IntegerType::new(context, 5);
    let raised = read_field(&mut graph, environment, ResourceField::FpFlags, flags_ty);
    let snapshot = resource::value(&mut graph, operation, operation.value_operands()[0]);
    let hi = resource::constant(&mut graph, 13, 4);
    let lo = resource::constant(&mut graph, 13, 0);
    let saved_flags = resource::operation(&mut graph, SymKind::Extract, &[snapshot, hi, lo]);
    graph.annotation_mut(saved_flags).actual_type = Some(flags_ty);
    let flags = resource::operation(&mut graph, SymKind::Or, &[saved_flags, raised]);
    graph.annotation_mut(flags).actual_type = Some(flags_ty);
    let environment = restore_snapshot(
        &mut graph,
        environment,
        snapshot,
        context.get_value(environment_value).ty(),
    );
    store_flags(
        operation,
        graph,
        environment,
        flags,
        raised,
        crate::fp::update_trapping(operation),
    )
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_UpdateOp_HasResourceSemantics: crate::HasResourceSemanticsCallbacks =
    crate::HasResourceSemanticsCallbacks {
        resource_semantics: crate::fp::update_hasresourcesemantics_resource_semantics,
    };

pub(crate) fn rounding_constant_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    if context
        .get_value(crate::fp::rounding_constant_result(operation))
        .ty()
        != RoundingType::new(context)
        || parse_rounding(&crate::fp::rounding_constant_mode(operation)).is_none()
    {
        return Err(Error::VerificationError(
            "fp.rounding_constant requires a known logical rounding mode".into(),
        ));
    }
    Ok(())
}

fn trapping_verify(operation: &tir::OpHandle, _context: &Context) -> Result<(), Error> {
    verify_ports(
        operation,
        &required_effects(ResourceAccess::Change, trapping(operation)),
    )
}

fn trapping_resource_effects(operation: &tir::OpHandle) -> Vec<ResourceEffect> {
    effect_records(
        operation,
        required_effects(ResourceAccess::Change, trapping(operation)),
    )
}

fn trapping(operation: &tir::OpHandle) -> bool {
    match crate::Operation::attr(operation, "trapping") {
        Some(crate::attributes::AttributeValue::Bool(value)) => value,
        _ => panic!("floating environment operation must carry its trapping attribute"),
    }
}

fn mask(value: &crate::interp::Value) -> Result<u8, crate::interp::InterpError> {
    let crate::interp::Value::Int(value) = value else {
        return Err(crate::interp::InterpError::Message(
            "flag mask must be !i5".into(),
        ));
    };
    Ok(value.to_u64() as u8)
}

pub(crate) fn rounding_constant_interp_evaluate(
    operation: &tir::OpHandle,
    _: &[crate::interp::Value],
    _: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    Ok(vec![crate::interp::Value::Rounding(
        parse_rounding(&crate::fp::rounding_constant_mode(operation)).expect("verified mode"),
    )])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_RoundingConstantOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::rounding_constant_interp_evaluate,
    };

pub(crate) fn raise_flags_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    state: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let raised = mask(&operands[0])?;
    state.fp_environment.flags |= raised;
    resource::check_trap(
        raised,
        state.fp_environment.traps,
        crate::fp::raise_flags_trapping(operation),
    )?;
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_RaiseFlagsOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::raise_flags_interp_evaluate,
    };

pub(crate) fn update_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    state: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    let [crate::interp::Value::FpEnvironment(snapshot)] = operands else {
        return Err(crate::interp::InterpError::Message(
            "fp.update requires !fp.env".into(),
        ));
    };
    let raised = state.fp_environment.flags;
    state.fp_environment = *snapshot;
    state.fp_environment.flags |= raised;
    resource::check_trap(
        raised,
        state.fp_environment.traps,
        crate::fp::update_trapping(operation),
    )?;
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_UpdateOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::update_interp_evaluate,
    };
