use crate as tir;
use crate::attributes::AttributeValue;
use crate::binding;
use crate::builtin::{FloatType, StateResource};
use crate::parse::common::Cursor;
use crate::{
    Binding, Context, Error, Operation, RegionBinding, ResourceAccess, ResourceEffect,
    ResourceEffects, ValueId,
};

tir::helpers::op_def! {
    file: "defs/converted/dialects_fp_round.cdsl",
    hosts: {
        round_verifier => crate::fp::round_verify_impl,
        round_iface_RegionBinding => (RegionBinding, &crate::fp::__TIR_IFACE_RoundOp_RegionBinding),
        round_iface_ResourceEffects => (ResourceEffects, &crate::fp::__TIR_IFACE_RoundOp_ResourceEffects),
        round_printer => crate::fp::round_custom_print,
        round_parser => |parser, context, _definition| crate::fp::round_custom_parse(parser, context),
        fence_iface_Interp => (crate :: interp :: Interp, &crate::fp::__TIR_IFACE_FenceOp_Interp),
        fence_iface_SameOperandAndResultType => (crate :: SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
    },
}

pub(crate) fn fence_interp_evaluate(
    _operation: &tir::OpHandle,
    operands: &[crate::interp::Value],
    _state: &mut crate::interp::ExecutionState,
) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
    Ok(vec![operands[0].clone()])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_FenceOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::fp::fence_interp_evaluate,
    };

pub(crate) fn round_regionbinding_region(operation: &tir::OpHandle) -> tir::RegionId {
    crate::fp::round_reference_region(operation).id()
}

pub(crate) fn round_regionbinding_binding(operation: &tir::OpHandle) -> Binding {
    let captures = operation.operands().len();
    let results = operation.results().len();
    Binding {
        operands: 0..captures,
        ports: 0..captures,
        continue_: 0..0,
        exit: 0..results,
        results: 0..results,
    }
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_RoundOp_RegionBinding: crate::RegionBindingCallbacks =
    crate::RegionBindingCallbacks {
        region: crate::fp::round_regionbinding_region,
        binding: crate::fp::round_regionbinding_binding,
    };

pub(crate) fn round_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    let context = &operation.context;
    super::resource::effects_for(
        Some(crate::fp::round_contract(operation).arithmetic.rounding),
        crate::fp::round_contract(operation).arithmetic.exceptions,
    )
    .into_iter()
    .map(|(resource, access)| ResourceEffect {
        resource,
        access,
        observed: context.states_of(operation.operands(), resource),
        produced: context.states_of(operation.results(), resource),
    })
    .collect()
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_RoundOp_ResourceEffects: crate::ResourceEffectsCallbacks =
    crate::ResourceEffectsCallbacks {
        resource_effects: crate::fp::round_resourceeffects_resource_effects,
    };

pub fn round_reference_region(operation: &tir::OpHandle) -> tir::RegionHandle {
    operation.context.get_region(
        *operation
            .regions()
            .first()
            .expect("fp.round owns its reference"),
    )
}

pub(crate) fn round_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.handle().context.clone();
    let ports: Vec<_> = crate::fp::round_reference_region(operation)
        .ports()
        .iter()
        .map(tir::Value::id)
        .collect();
    tir::region_format::print_result_prefix(fmt, operation)?;
    fmt.write("fp.round")?;
    binding::print_port_bindings(fmt, &ports, &operation.operands())?;
    fmt.write(" {contract = ")?;
    AttributeValue::EvaluationContract(crate::fp::round_contract(operation))
        .print(fmt, &context)?;
    fmt.write("} : ")?;
    print_types(
        fmt,
        &context,
        &operation
            .results()
            .iter()
            .map(|result| context.get_value(*result).ty())
            .collect::<Vec<_>>(),
    )?;
    tir::region_format::print_region(fmt, &context, &crate::fp::round_reference_region(operation))
}

pub(crate) fn round_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
    let bound = binding::parse_port_bindings(parser, context)?;
    binding::expect(parser, "{")?;
    binding::expect(parser, "contract")?;
    binding::expect(parser, "=")?;
    let attribute = parser
        .parse_attribute_value(context)?
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("contract")))?;
    let contract = super::EvaluationContract::parse_attribute(&attribute)
        .map_err(|error| (parser.span(), error))?;
    binding::expect(parser, "}")?;
    binding::expect(parser, ":")?;
    let result_types = parse_types(parser, context)?;
    let reference = parser
        .parse_region_with_entry_args(context, bound.ports)?
        .id();
    Ok(r#round(
        context,
        bound.inits,
        tir::attributes::AttributeValue::EvaluationContract(
            context.intern_evaluation_contract(contract),
        ),
        result_types,
        Some(reference),
        Default::default(),
        tir::InsertionPoint::Root,
    ))
}

pub(crate) fn round_verify_impl(operation: &tir::OpHandle, context: &Context) -> Result<(), Error> {
    let ports = crate::fp::round_reference_region(operation).ports();
    let captures = operation.operands();
    let yielded = crate::fp::round_reference_region(operation).results();
    let results = operation.results();
    if ports.len() != captures.len() {
        return Err(invalid(
            "fp.round must bind every capture to one reference port",
        ));
    }
    if yielded.len() != results.len() {
        return Err(invalid(
            "fp.round reference yield count must match its result count",
        ));
    }
    for (index, (port, capture)) in ports.iter().zip(captures).enumerate() {
        if port.ty() != context.get_value(capture).ty() {
            return Err(invalid(&format!(
                "fp.round port {index} must have its capture's type"
            )));
        }
    }
    for (index, (yielded, result)) in yielded.iter().zip(results.iter()).enumerate() {
        if context.get_value(*yielded).ty() != context.get_value(*result).ty() {
            return Err(invalid(&format!(
                "fp.round yield {index} must have its result's type"
            )));
        }
    }
    let formats: Vec<_> = results
        .iter()
        .filter_map(|result| {
            let ty = context.get_value(*result).ty();
            (!context.is_state_type(ty)).then_some(ty)
        })
        .collect();
    if crate::fp::round_contract(operation).result_formats != formats {
        return Err(invalid(
            "fp.round result formats must match its numeric results",
        ));
    }
    if results.iter().any(|result| {
        let ty = context.get_value(*result).ty();
        !context.is_state_type(ty) && FloatType::view(context, ty).is_none()
    }) {
        return Err(invalid("fp.round numeric results must have floating types"));
    }
    verify_explicit_captures(context, crate::fp::round_reference_region(operation).id())?;
    verify_primitive_semantics(
        context,
        crate::fp::round_reference_region(operation).id(),
        crate::fp::round_contract(operation).arithmetic,
    )?;
    let expected = super::resource::effects_for(
        Some(crate::fp::round_contract(operation).arithmetic.rounding),
        crate::fp::round_contract(operation).arithmetic.exceptions,
    );
    verify_body_effects(
        context,
        crate::fp::round_reference_region(operation).id(),
        &expected,
    )?;
    for resource in [StateResource::Memory, StateResource::FpEnv] {
        let count = usize::from(expected.iter().any(|(candidate, _)| *candidate == resource));
        if context.states_of(operation.operands(), resource).len() != count
            || context.states_of(results.iter().copied(), resource).len() != count
        {
            return Err(invalid("fp.round state ports do not match its contract"));
        }
        if expected
            .iter()
            .any(|&(candidate, access)| candidate == resource && access == ResourceAccess::Change)
        {
            verify_changed_state_is_yielded(
                context,
                crate::fp::round_reference_region(operation).id(),
                &yielded,
                resource,
            )?;
        }
    }
    Ok(())
}

fn parse_types(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Vec<tir::TypeId>, (tir::parse::Span, Error)> {
    if parser.parse_token("(") {
        let mut types = Vec::new();
        loop {
            types.push(
                parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?,
            );
            if parser.parse_token(")") {
                break;
            }
            binding::expect(parser, ",")?;
        }
        Ok(types)
    } else {
        Ok(vec![
            parser
                .parse_type(context)?
                .ok_or_else(|| (parser.span(), Error::ExpectedType))?,
        ])
    }
}

fn print_types(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    types: &[tir::TypeId],
) -> Result<(), std::fmt::Error> {
    if types.len() > 1 {
        fmt.write("(")?;
    }
    for (index, ty) in types.iter().enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        context.print_type(*ty, fmt)?;
    }
    if types.len() > 1 {
        fmt.write(")")?;
    }
    Ok(())
}

fn invalid(message: &str) -> Error {
    Error::VerificationError(message.into())
}

fn verify_explicit_captures(context: &Context, region: tir::RegionId) -> Result<(), Error> {
    let handle = context.get_region(region);
    let mut local: std::collections::HashSet<_> =
        handle.ports().iter().map(tir::Value::id).collect();
    collect_defined(context, region, &mut local);
    for op in handle.op_ids() {
        for value in crate::region::values_read(context, op) {
            if !local.contains(&value) {
                return Err(invalid(
                    "fp.round reference may read outer values only through explicit capture ports",
                ));
            }
        }
    }
    if handle.results().iter().any(|value| !local.contains(value)) {
        return Err(invalid(
            "fp.round reference may yield outer values only through explicit capture ports",
        ));
    }
    Ok(())
}

fn collect_defined(
    context: &Context,
    region: tir::RegionId,
    values: &mut std::collections::HashSet<ValueId>,
) {
    let handle = context.get_region(region);
    values.extend(handle.ports().iter().map(tir::Value::id));
    for op in handle.op_ids() {
        let op = context.get_op(op);
        values.extend(op.results());
        for nested in op.regions() {
            collect_defined(context, nested, values);
        }
    }
}

fn verify_primitive_semantics(
    context: &Context,
    region: tir::RegionId,
    expected: super::ArithmeticSemantics,
) -> Result<(), Error> {
    for op in context.get_region(region).op_ids() {
        let op = context.get_op(op);
        if op.is_operation(&ROUND) {
            continue;
        }
        if let Some(AttributeValue::FpSemantics(semantics)) = op.attr("semantics")
            && semantics
                .arithmetic()
                .is_ok_and(|actual| *actual != expected)
        {
            return Err(invalid(
                "fp.round primitive semantics must match its contract",
            ));
        }
        for nested in op.regions() {
            verify_primitive_semantics(context, nested, expected)?;
        }
    }
    Ok(())
}

fn verify_body_effects(
    context: &Context,
    region: tir::RegionId,
    allowed: &[(StateResource, ResourceAccess)],
) -> Result<(), Error> {
    for op in context.get_region(region).op_ids() {
        let op = context.get_op(op);
        if let Some(effects) = op.clone().as_interface::<dyn ResourceEffects>() {
            for effect in effects.resource_effects() {
                let admitted = allowed.iter().any(|(resource, access)| {
                    *resource == effect.resource
                        && (*access == ResourceAccess::Change
                            || effect.access == ResourceAccess::Read)
                });
                if !admitted {
                    return Err(invalid(
                        "fp.round reference has a state effect absent from its contract",
                    ));
                }
            }
        }
        for nested in op.regions() {
            verify_body_effects(context, nested, allowed)?;
        }
    }
    Ok(())
}

fn verify_changed_state_is_yielded(
    context: &Context,
    region: tir::RegionId,
    yielded: &[ValueId],
    resource: StateResource,
) -> Result<(), Error> {
    let mut predecessors = std::collections::HashMap::<ValueId, Vec<ValueId>>::new();
    let mut changed = std::collections::HashSet::new();
    collect_state_effects(context, region, resource, &mut predecessors, &mut changed);

    let mut reachable = context.states_of(yielded.iter().copied(), resource);
    let mut visited = std::collections::HashSet::new();
    while let Some(state) = reachable.pop() {
        if visited.insert(state)
            && let Some(previous) = predecessors.get(&state)
        {
            reachable.extend(previous);
        }
    }
    if !changed.is_subset(&visited) {
        return Err(invalid(
            "fp.round reference must yield the final state of its body effects",
        ));
    }
    Ok(())
}

fn collect_state_effects(
    context: &Context,
    region: tir::RegionId,
    resource: StateResource,
    predecessors: &mut std::collections::HashMap<ValueId, Vec<ValueId>>,
    changed: &mut std::collections::HashSet<ValueId>,
) {
    for op in context.get_region(region).op_ids() {
        let op = context.get_op(op);
        if let Some(effects) = op.clone().as_interface::<dyn ResourceEffects>() {
            for effect in effects
                .resource_effects()
                .into_iter()
                .filter(|effect| effect.resource == resource)
            {
                for produced in &effect.produced {
                    predecessors
                        .entry(*produced)
                        .or_default()
                        .extend(&effect.observed);
                }
                if effect.access == ResourceAccess::Change {
                    changed.extend(effect.produced);
                }
            }
        }
        for chain in binding::state_chains(context, &op) {
            if context.state_resource(context.get_value(chain.left).ty()) != Some(resource) {
                continue;
            }
            predecessors
                .entry(chain.left)
                .or_default()
                .extend(&chain.exits);
            for port in chain.ports {
                predecessors.entry(port).or_default().push(chain.entered);
            }
            if let Some(next) = chain.next {
                predecessors.entry(next).or_default().extend(&chain.exits);
            }
        }
        if op.is_operation(&ROUND) {
            continue;
        }
        for nested in op.regions() {
            collect_state_effects(context, nested, resource, predecessors, changed);
        }
    }
}
