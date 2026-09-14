use crate as tir;
use crate::attributes::AttributeValue;
use crate::binding;
use crate::builtin::{FloatType, StateResource};
use crate::parse::common::Cursor;
use crate::{
    Binding, Context, Error, Operation, RegionBinding, ResourceAccess, ResourceEffect,
    ResourceEffects, ValueId, operation,
};

operation! {
    RoundOp {
        name: "round",
        dialect: "fp",
        format: "custom",
        verifier: "true",
        operands: O { captures: "*tir::Any" },
        attributes: A { contract: "EvaluationContract" },
        results: R { results: "*tir::Any" },
        regions: R { reference: Region { kind: Nodes } },
        interfaces: [RegionBinding, ResourceEffects],
    }
}

operation! {
    FenceOp {
        name: "fence",
        dialect: "fp",
        operands: O { input: "FloatType" },
        results: R { result: "FloatType" },
        interfaces: [crate::interp::Interp, crate::SameOperandAndResultType],
    }
}

impl crate::interp::Interp for FenceOp {
    fn evaluate(
        &self,
        operands: &[crate::interp::Value],
        _state: &mut crate::interp::ExecutionState,
    ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
        Ok(vec![operands[0].clone()])
    }
}

impl crate::SameOperandAndResultType for FenceOp {}

impl RegionBinding for RoundOp {
    fn region(&self) -> tir::RegionId {
        self.reference_region().id()
    }

    fn binding(&self) -> Binding {
        let captures = self.operands().len();
        let results = self.0.results().len();
        Binding {
            operands: 0..captures,
            ports: 0..captures,
            continue_: 0..0,
            exit: 0..results,
            results: 0..results,
        }
    }
}

impl ResourceEffects for RoundOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        let context = &self.0.context;
        super::resource::effects_for(
            Some(self.contract().arithmetic.rounding),
            self.contract().arithmetic.exceptions,
        )
        .into_iter()
        .map(|(resource, access)| ResourceEffect {
            resource,
            access,
            observed: states_for(context, &self.operands(), resource),
            produced: states_for(context, &self.0.results(), resource),
        })
        .collect()
    }
}

impl RoundOp {
    pub fn reference_region(&self) -> tir::RegionHandle {
        self.regions().next().expect("fp.round owns its reference")
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.handle().context.clone();
        let ports: Vec<_> = self
            .reference_region()
            .ports()
            .iter()
            .map(tir::Value::id)
            .collect();
        tir::region_format::print_result_prefix(fmt, &self.0)?;
        fmt.write("fp.round")?;
        binding::print_port_bindings(fmt, &ports, &self.operands())?;
        fmt.write(" {contract = ")?;
        AttributeValue::EvaluationContract(self.contract()).print(fmt, &context)?;
        fmt.write("} : ")?;
        print_types(
            fmt,
            &context,
            &self
                .0
                .results()
                .iter()
                .map(|result| context.get_value(*result).ty())
                .collect::<Vec<_>>(),
        )?;
        tir::region_format::print_region(fmt, &context, &self.reference_region())
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
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
        Ok(Box::new(
            RoundOpBuilder::new(context)
                .captures(bound.inits)
                .contract(context.intern_evaluation_contract(contract))
                .result_types(result_types)
                .reference(reference)
                .build(),
        ))
    }
}

impl tir::Verifiable for RoundOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let ports = self.reference_region().ports();
        let captures = self.operands();
        let yielded = self.reference_region().results();
        let results = self.0.results();
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
        if self.contract().result_formats != formats {
            return Err(invalid(
                "fp.round result formats must match its numeric results",
            ));
        }
        if results.iter().any(|result| {
            let ty = context.get_type_data(context.get_value(*result).ty());
            !context.is_state_type(context.get_value(*result).ty())
                && (ty.as_ref() as &dyn std::any::Any)
                    .downcast_ref::<FloatType>()
                    .is_none()
        }) {
            return Err(invalid("fp.round numeric results must have floating types"));
        }
        verify_explicit_captures(context, self.reference_region().id())?;
        verify_primitive_semantics(
            context,
            self.reference_region().id(),
            self.contract().arithmetic,
        )?;
        let expected = super::resource::effects_for(
            Some(self.contract().arithmetic.rounding),
            self.contract().arithmetic.exceptions,
        );
        verify_body_effects(context, self.reference_region().id(), &expected)?;
        for resource in [StateResource::Memory, StateResource::FpEnv] {
            let count = usize::from(expected.iter().any(|(candidate, _)| *candidate == resource));
            if states_for(context, &self.operands(), resource).len() != count
                || states_for(context, &results, resource).len() != count
            {
                return Err(invalid("fp.round state ports do not match its contract"));
            }
            if expected.iter().any(|&(candidate, access)| {
                candidate == resource && access == ResourceAccess::Change
            }) {
                verify_changed_state_is_yielded(
                    context,
                    self.reference_region().id(),
                    &yielded,
                    resource,
                )?;
            }
        }
        Ok(())
    }
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

fn states_for(context: &Context, values: &[ValueId], resource: StateResource) -> Vec<ValueId> {
    values
        .iter()
        .copied()
        .filter(|value| context.state_resource(context.get_value(*value).ty()) == Some(resource))
        .collect()
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
        if op.is::<RoundOp>() {
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

    let mut reachable = states_for(context, yielded, resource);
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
        if op.is::<RoundOp>() {
            continue;
        }
        for nested in op.regions() {
            collect_state_effects(context, nested, resource, predecessors, changed);
        }
    }
}
