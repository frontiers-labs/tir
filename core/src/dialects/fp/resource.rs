use tir_adt::APInt;

use super::{ArithmeticSemantics, Exceptions, IntegerConversionSemantics, NaNPolicy, Rounding};
use crate::NodeId;
use crate::builtin::{IntegerType, StateResource};
use crate::sem::{SemGraph, SymKind, SymPayload};
use crate::{
    Error, OpHandle, ResourceAccess, ResourceEffect, ResourceField, ResourceSemantics, ValueId,
};

pub(super) fn effects_for(
    rounding: Option<Rounding>,
    exceptions: Exceptions,
) -> Vec<(StateResource, ResourceAccess)> {
    let mut effects = Vec::new();
    match (rounding, exceptions) {
        (None | Some(Rounding::Fixed(_)), Exceptions::Ignore) => {}
        (Some(Rounding::Dynamic), Exceptions::Ignore) => {
            effects.push((StateResource::FpEnv, ResourceAccess::Read));
        }
        (_, Exceptions::Flags | Exceptions::FlagsAndTraps) => {
            effects.push((StateResource::FpEnv, ResourceAccess::Change));
        }
    }
    if exceptions == Exceptions::FlagsAndTraps {
        effects.push((StateResource::Memory, ResourceAccess::Change));
    }
    effects
}

pub(super) fn value(graph: &mut SemGraph, op: &OpHandle, value: ValueId) -> NodeId {
    let node = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(node, SymPayload::Value(value));
    graph.annotation_mut(node).actual_type = Some(op.context.get_value(value).ty());
    node
}

pub(super) fn constant(graph: &mut SemGraph, width: u32, value: u64) -> NodeId {
    let node = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(node, SymPayload::Int(APInt::new(width, value)));
    node
}

pub(super) fn operation(graph: &mut SemGraph, kind: SymKind, children: &[NodeId]) -> NodeId {
    let node = graph.add_node(kind);
    for &child in children {
        graph.add_edge(node, child);
    }
    node
}

pub(super) fn state_for(op: &OpHandle, resource: StateResource) -> ValueId {
    op.state_operands()
        .into_iter()
        .find(|value| {
            op.context.state_resource(op.context.get_value(*value).ty()) == Some(resource)
        })
        .expect("verified floating operation has its required resource state")
}

fn states_for(
    op: &OpHandle,
    values: impl IntoIterator<Item = ValueId>,
    resource: StateResource,
) -> Vec<ValueId> {
    values
        .into_iter()
        .filter(|value| {
            op.context.state_resource(op.context.get_value(*value).ty()) == Some(resource)
        })
        .collect()
}

pub(super) fn verify_ports(
    op: &OpHandle,
    effects: &[(StateResource, ResourceAccess)],
) -> Result<(), Error> {
    if op.state_operands().len() != effects.len() || op.state_results().len() != effects.len() {
        return Err(Error::VerificationError(
            "floating operation state ports do not match its semantics".into(),
        ));
    }
    for &(resource, _) in effects {
        if [op.state_operands(), op.state_results()]
            .into_iter()
            .any(|values| states_for(op, values, resource).len() != 1)
        {
            return Err(Error::VerificationError(
                "floating operation has a mismatched state resource".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn effect_records(
    op: &OpHandle,
    required: impl IntoIterator<Item = (StateResource, ResourceAccess)>,
) -> Vec<ResourceEffect> {
    required
        .into_iter()
        .map(|(resource, access)| ResourceEffect {
            resource,
            access,
            observed: states_for(op, op.state_operands(), resource),
            produced: states_for(op, op.state_results(), resource),
        })
        .collect()
}

pub(super) fn apply_flags(
    result: tir_adt::FloatResult,
    exceptions: Exceptions,
    state: &mut crate::interp::ExecutionState,
) -> Result<tir_adt::FloatResult, crate::interp::InterpError> {
    if exceptions != Exceptions::Ignore {
        state.fp_environment.flags |= result.flags;
    }
    check_trap(
        result.flags,
        state.fp_environment.traps,
        exceptions == Exceptions::FlagsAndTraps,
    )?;
    Ok(result)
}

pub(super) fn check_trap(
    raised: u8,
    traps: u8,
    trapping: bool,
) -> Result<(), crate::interp::InterpError> {
    if trapping && raised & traps != 0 {
        Err(crate::interp::InterpError::Message(
            "floating-point trap".into(),
        ))
    } else {
        Ok(())
    }
}

pub(super) fn resource(graph: &mut SemGraph, resource: StateResource) -> NodeId {
    constant(graph, 2, resource.semantic_code())
}

pub(super) fn field(graph: &mut SemGraph, field: ResourceField) -> NodeId {
    constant(graph, 2, field.semantic_code())
}

fn access(graph: &mut SemGraph, access: ResourceAccess) -> NodeId {
    constant(graph, 1, access.semantic_code())
}

pub(super) fn state_read(
    graph: &mut SemGraph,
    state: NodeId,
    resource: NodeId,
    field: NodeId,
    ty: crate::TypeId,
) -> NodeId {
    let read = operation(graph, SymKind::StateRead, &[state, resource, field]);
    graph.annotation_mut(read).actual_type = Some(ty);
    read
}

pub(super) fn state_assign(
    graph: &mut SemGraph,
    state: NodeId,
    resource: NodeId,
    field: NodeId,
    access_kind: ResourceAccess,
    value: NodeId,
    ty: crate::TypeId,
) -> NodeId {
    let access = access(graph, access_kind);
    let assigned = operation(
        graph,
        SymKind::StateAssign,
        &[state, resource, field, access, value],
    );
    graph.annotation_mut(assigned).actual_type = Some(ty);
    assigned
}

fn arithmetic_value(
    graph: &mut SemGraph,
    op: &OpHandle,
    generic: SymKind,
    rounded: SymKind,
    result_format: Option<(u32, u32)>,
    semantics: &ArithmeticSemantics,
    environment: Option<NodeId>,
) -> (NodeId, NodeId, Option<NodeId>) {
    let mut operands: Vec<_> = op
        .value_operands()
        .into_iter()
        .map(|operand| value(graph, op, operand))
        .collect();
    if let Some((exponent, mantissa)) = result_format {
        for width in [exponent, mantissa] {
            operands.push(constant(
                graph,
                (u32::BITS - width.leading_zeros()).max(1),
                width as u64,
            ));
        }
    }
    let result_ty = op.context.get_value(op.value_results()[0]).ty();
    let uses_generic = semantics.exceptions == Exceptions::Ignore
        && semantics.nan == NaNPolicy::AnyQuiet
        && semantics.rounding == Rounding::Fixed(tir_adt::RoundingMode::TiesToEven);
    let rounding = match semantics.rounding {
        Rounding::Fixed(mode) if !uses_generic => {
            Some(constant(graph, 3, super::semantics::rounding_code(mode)))
        }
        Rounding::Fixed(_) => None,
        Rounding::Dynamic => {
            let state = environment.expect("dynamic rounding requires FP environment state");
            let resource = resource(graph, StateResource::FpEnv);
            let field = field(graph, ResourceField::FpRounding);
            Some(state_read(
                graph,
                state,
                resource,
                field,
                super::RoundingType::new(&op.context),
            ))
        }
    };
    let canonical_bits = (semantics.nan == NaNPolicy::Canonical).then(|| {
        let width = super::arithmetic::float_width(&op.context, result_ty)
            .expect("verified FP arithmetic has a floating result");
        let bits = constant(
            graph,
            width.bit_width(),
            super::arithmetic::canonical_nan(width),
        );
        graph.annotation_mut(bits).actual_type =
            Some(IntegerType::new(&op.context, width.bit_width()));
        bits
    });
    let (result, evaluation) = super::arithmetic::build_arithmetic_value(
        graph,
        &operands,
        (generic, rounded),
        uses_generic,
        rounding,
        semantics.nan,
        canonical_bits,
    );
    graph.annotation_mut(evaluation).actual_type = Some(result_ty);
    graph.annotation_mut(result).actual_type = Some(result_ty);
    (result, evaluation, rounding)
}

pub(super) fn canonicalize_nan(
    graph: &mut SemGraph,
    value: NodeId,
    canonical_bits: NodeId,
) -> NodeId {
    let non_nan = operation(graph, SymKind::Ge, &[value, value]);
    let canonical = operation(graph, SymKind::AsFloat, &[canonical_bits]);
    operation(graph, SymKind::If, &[non_nan, value, canonical])
}

pub(super) fn transition_semantics(
    op: &OpHandle,
    value_result: Option<NodeId>,
    environment: NodeId,
    memory: Option<NodeId>,
    mut graph: SemGraph,
) -> ResourceSemantics {
    let root = match (value_result, memory) {
        (Some(value), _) => value,
        (None, Some(memory)) => operation(&mut graph, SymKind::StateBlock, &[environment, memory]),
        (None, None) => environment,
    };
    let state_results = op
        .state_results()
        .into_iter()
        .map(
            |result| match op.context.state_resource(op.context.get_value(result).ty()) {
                Some(StateResource::FpEnv) => environment,
                Some(StateResource::Memory) => memory.expect("trapping transition has memory"),
                None => unreachable!("state result has a state resource"),
            },
        )
        .collect();
    ResourceSemantics {
        graph,
        raised_flags: None,
        root,
        value_results: value_result.into_iter().collect(),
        state_results,
    }
}

pub(super) fn flagged_result(
    op: &OpHandle,
    mut graph: SemGraph,
    result: NodeId,
    raised: NodeId,
    exceptions: Exceptions,
    environment: NodeId,
) -> ResourceSemantics {
    let environment_value = state_for(op, StateResource::FpEnv);
    let environment_ty = op.context.get_value(environment_value).ty();
    let fp_resource = resource(&mut graph, StateResource::FpEnv);
    let flags_field = field(&mut graph, ResourceField::FpFlags);
    let flags_ty = IntegerType::new(&op.context, 5);
    let old = state_read(&mut graph, environment, fp_resource, flags_field, flags_ty);
    let flags = operation(&mut graph, SymKind::Or, &[old, raised]);
    graph.annotation_mut(flags).actual_type = Some(flags_ty);
    let environment = state_assign(
        &mut graph,
        environment,
        fp_resource,
        flags_field,
        ResourceAccess::Change,
        flags,
        environment_ty,
    );
    let memory = (exceptions == Exceptions::FlagsAndTraps).then(|| {
        let memory_value = state_for(op, StateResource::Memory);
        let memory = value(&mut graph, op, memory_value);
        let traps_field = field(&mut graph, ResourceField::FpTraps);
        let traps = state_read(&mut graph, environment, fp_resource, traps_field, flags_ty);
        let trap = operation(&mut graph, SymKind::StateTrap, &[memory, raised, traps]);
        graph.annotation_mut(trap).actual_type = Some(op.context.get_value(memory_value).ty());
        trap
    });
    let mut semantics = transition_semantics(op, Some(result), environment, memory, graph);
    semantics.raised_flags = Some(raised);
    semantics
}

fn finish_rounded(
    op: &OpHandle,
    mut graph: SemGraph,
    result: NodeId,
    outcome: NodeId,
    environment: Option<NodeId>,
    rounding_read: Option<NodeId>,
    exceptions: Exceptions,
) -> ResourceSemantics {
    if matches!(exceptions, Exceptions::Flags | Exceptions::FlagsAndTraps) {
        let raised = operation(&mut graph, SymKind::FPFlags, &[outcome]);
        graph.annotation_mut(raised).actual_type = Some(IntegerType::new(&op.context, 5));
        return flagged_result(
            op,
            graph,
            result,
            raised,
            exceptions,
            environment.expect("observable flags require FP environment state"),
        );
    }

    let environment_out = environment.map(|state| {
        let resource = resource(&mut graph, StateResource::FpEnv);
        let field = field(&mut graph, ResourceField::FpRounding);
        let environment_value = state_for(op, StateResource::FpEnv);
        state_assign(
            &mut graph,
            state,
            resource,
            field,
            ResourceAccess::Read,
            rounding_read.expect("dynamic rounding has a rounding read"),
            op.context.get_value(environment_value).ty(),
        )
    });
    ResourceSemantics {
        graph,
        raised_flags: None,
        root: result,
        value_results: vec![result],
        state_results: environment_out.into_iter().collect(),
    }
}

pub(super) fn arithmetic(
    op: &OpHandle,
    generic: SymKind,
    rounded: SymKind,
    result_format: Option<(u32, u32)>,
    semantics: &ArithmeticSemantics,
) -> ResourceSemantics {
    let mut graph = SemGraph::new();
    let environment_value = (!matches!(
        (semantics.rounding, semantics.exceptions),
        (Rounding::Fixed(_), Exceptions::Ignore)
    ))
    .then(|| state_for(op, StateResource::FpEnv));
    let environment = environment_value.map(|state| value(&mut graph, op, state));
    let (result, rounded, rounding) = arithmetic_value(
        &mut graph,
        op,
        generic,
        rounded,
        result_format,
        semantics,
        environment,
    );

    finish_rounded(
        op,
        graph,
        result,
        rounded,
        environment,
        rounding,
        semantics.exceptions,
    )
}

pub(super) fn integer_conversion(
    op: &OpHandle,
    generic: SymKind,
    rounded: SymKind,
    semantics: &IntegerConversionSemantics,
) -> ResourceSemantics {
    let mut graph = SemGraph::new();
    let environment = (!matches!(
        (semantics.rounding, semantics.exceptions),
        (Rounding::Fixed(_), Exceptions::Ignore)
    ))
    .then(|| value(&mut graph, op, state_for(op, StateResource::FpEnv)));
    let input = value(&mut graph, op, op.value_operands()[0]);
    let result_ty = op.context.get_value(op.value_results()[0]).ty();
    let data = op.context.get_type_data(result_ty);
    let width = (data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()
        .expect("verified integer conversion result")
        .width();
    let width_node = constant(
        &mut graph,
        (u32::BITS - width.leading_zeros()).max(1),
        width as u64,
    );
    let rounding = match semantics.rounding {
        Rounding::Fixed(mode) => constant(&mut graph, 3, super::semantics::rounding_code(mode)),
        Rounding::Dynamic => {
            let resource = resource(&mut graph, StateResource::FpEnv);
            let field = field(&mut graph, ResourceField::FpRounding);
            state_read(
                &mut graph,
                environment.expect("dynamic rounding requires FP environment state"),
                resource,
                field,
                super::RoundingType::new(&op.context),
            )
        }
    };
    let outcome = operation(&mut graph, rounded, &[input, width_node, rounding]);
    let result = if semantics.rounding == Rounding::Fixed(tir_adt::RoundingMode::TowardZero) {
        operation(&mut graph, generic, &[input, width_node])
    } else {
        outcome
    };
    graph.annotation_mut(result).actual_type = Some(result_ty);
    finish_rounded(
        op,
        graph,
        result,
        outcome,
        environment,
        (semantics.rounding == Rounding::Dynamic).then_some(rounding),
        semantics.exceptions,
    )
}
