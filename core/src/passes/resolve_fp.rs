use crate::backend::{isel::InstructionSelectPass, select_target};
use crate::builtin::FloatType;
use crate::fp::ops::{FenceOp, FenceOpBuilder, FmaOpBuilder, NegOpBuilder, RoundOp};
use crate::func::FuncOp;
use crate::sem::fp_refinement::{
    CandidateSnapshot, ContractionCandidate, ContractionProposal, FusedForm, RefinementError,
    check_contraction, contraction_proposals,
};
use crate::{
    AnalysisManager, Context, OpId, Operation, OperationRef, Pass, PassError, PassTarget,
    RegionKind,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

pub struct ResolveFpPass {
    selector: Option<Arc<Mutex<InstructionSelectPass>>>,
    target: Option<String>,
}

impl ResolveFpPass {
    pub fn with_selector(selector: InstructionSelectPass) -> Self {
        Self {
            selector: Some(Arc::new(Mutex::new(selector))),
            target: None,
        }
    }

    pub(crate) fn shared(selector: Arc<Mutex<InstructionSelectPass>>) -> Self {
        Self {
            selector: Some(selector),
            target: None,
        }
    }

    fn for_target(target: String) -> Self {
        Self {
            selector: None,
            target: Some(target),
        }
    }

    fn selector(&self) -> MutexGuard<'_, InstructionSelectPass> {
        self.selector
            .as_ref()
            .expect("selector initialized")
            .lock()
            .expect("instruction select")
    }
}

fn parse_resolve_fp(arguments: &str) -> Result<ResolveFpPass, String> {
    if arguments.trim().is_empty() {
        return Err("resolve-fp requires a target, for example resolve-fp<rv64ifd>".into());
    }
    select_target(arguments.trim(), None, None)?;
    Ok(ResolveFpPass::for_target(arguments.trim().to_owned()))
}

crate::register_pass!(ResolveFpPass, "resolve-fp", parse_resolve_fp);

impl Pass for ResolveFpPass {
    fn name(&self) -> &'static str {
        "resolve-fp"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Nodes)
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if find_round(context, op.op().id).is_none() {
            return lower_fences(context, op.op().id);
        }
        if self.selector.is_none() {
            let target = select_target(self.target.as_deref().expect("parsed target"), None, None)
                .map_err(PassError::InvalidRuleSet)?;
            target.register_dialects(context);
            self.selector = Some(Arc::new(Mutex::new(target.isel_pass(context))));
        }
        self.resolve_all(context, op)?;
        lower_fences(context, op.op().id)
    }
}

impl ResolveFpPass {
    fn resolve_all(&mut self, context: &Context, function: &OperationRef) -> Result<(), PassError> {
        while let Some(round_id) = find_round(context, function.op().id) {
            self.resolve_one(context, function, round_id)?;
        }
        Ok(())
    }
    fn resolve_one(
        &mut self,
        context: &Context,
        function: &OperationRef,
        round_id: OpId,
    ) -> Result<(), PassError> {
        let round = context
            .get_op(round_id)
            .as_op::<RoundOp>()
            .expect("found fp.round");
        let candidates = contraction_candidates(context, &round)?;

        let mut best: Option<(u64, Context)> = None;
        let mut unsupported = Vec::new();
        let mut semantic_rejections = Vec::new();
        let mut unique_candidates = HashSet::new();
        if round.contract().reference_is_admissible() {
            let fork = context.fork();
            let fork_round = fork
                .get_op(round_id)
                .as_op::<RoundOp>()
                .expect("forked round");
            splice_reference(&fork, &fork_round)?;
            self.resolve_all(&fork, &OperationRef::new(fork.get_op(function.op().id)))?;
            lower_fences(&fork, function.op().id)?;
            match self
                .selector()
                .estimate_function_cost(&fork, &OperationRef::new(fork.get_op(function.op().id)))
            {
                Ok(cost) => best = Some((cost, fork)),
                Err(error) => unsupported.push(format!("reference: {error}")),
            }
        }

        for proposal in candidates {
            let fork = context.fork();
            let fork_round = fork
                .get_op(round_id)
                .as_op::<RoundOp>()
                .expect("forked round");
            let Ok(candidate) = materialize_candidate(&fork, &fork_round, &proposal) else {
                continue;
            };
            let snapshot = match CandidateSnapshot::capture(&fork, &fork_round, &candidate) {
                Ok(snapshot) => snapshot,
                Err(_) => continue,
            };
            if !unique_candidates.insert(snapshot) {
                continue;
            }
            let witness = match check_contraction(&fork, &fork_round, &candidate) {
                Ok(witness) => witness,
                Err(error) => {
                    let reason = refinement_rejection(error);
                    if !semantic_rejections.contains(&reason) {
                        semantic_rejections.push(reason);
                    }
                    continue;
                }
            };
            witness
                .validate(&fork, &fork_round, &candidate)
                .map_err(|_| PassError::InvalidRuleSet("stale FP refinement witness".into()))?;
            select_candidate(&fork, &fork_round, &candidate)?;
            self.resolve_all(&fork, &OperationRef::new(fork.get_op(function.op().id)))?;
            lower_fences(&fork, function.op().id)?;
            match self
                .selector()
                .estimate_function_cost(&fork, &OperationRef::new(fork.get_op(function.op().id)))
            {
                Ok(cost)
                    if best
                        .as_ref()
                        .is_none_or(|(best_cost, ..)| cost < *best_cost) =>
                {
                    best = Some((cost, fork));
                }
                Ok(_) => {}
                Err(error) => unsupported.push(error.to_string()),
            }
        }
        let Some((_, fork)) = best else {
            let reason = if !unsupported.is_empty() {
                format!(
                    "target has no complete FP candidate coverage: {}",
                    unsupported.join("; ")
                )
            } else if !semantic_rejections.is_empty() {
                format!("no legal FP refinement: {}", semantic_rejections.join("; "))
            } else {
                "no legal evaluation satisfies the fp.round contract".into()
            };
            return Err(PassError::InvalidRuleSet(reason));
        };
        context.adopt(fork);
        Ok(())
    }
}

fn refinement_rejection(error: RefinementError) -> &'static str {
    match error {
        RefinementError::StaleOperation => "candidate contains a stale operation",
        RefinementError::OutsideReference => "candidate leaves the fp.round reference",
        RefinementError::DisconnectedGroup => "candidate does not form a connected contraction",
        RefinementError::IncompatibleSemantics => "candidate has incompatible FP semantics",
        RefinementError::ContractForbidsContraction => "contract forbids contraction",
        RefinementError::IntermediateExceptionsObserved => {
            "candidate changes observed intermediate exceptions"
        }
        RefinementError::IncompleteResultMapping => "candidate does not map every result",
        RefinementError::UnsupportedAccuracy => {
            "unsupported FP accuracy requirement for contraction"
        }
        RefinementError::UnsupportedForm => "unsupported FP contraction form",
    }
}

fn lower_fences(context: &Context, function: OpId) -> Result<(), PassError> {
    if find_round(context, function).is_some() {
        return Ok(());
    }
    let fences: Vec<_> = super::regions_under(context, function)
        .into_iter()
        .flat_map(|region| context.get_region(region).op_ids())
        .filter(|op| context.get_op(*op).is::<FenceOp>())
        .collect();
    for fence_id in fences {
        let fence = context.get_op(fence_id);
        context.replace_value_uses(fence.results()[0], fence.operands()[0]);
        replace_region_result(context, fence_id, fence.results()[0], fence.operands()[0]);
        context.erase_op(&OperationRef::new(fence))?;
    }
    Ok(())
}

fn find_round(context: &Context, function: OpId) -> Option<OpId> {
    super::regions_under(context, function)
        .into_iter()
        .flat_map(|region| context.get_region(region).op_ids())
        .filter(|op| context.get_op(*op).is::<RoundOp>())
        .max_by_key(|op| {
            let mut depth = 0;
            let mut parent = context.parent_op(*op);
            while let Some(op) = parent {
                depth += 1;
                parent = context.parent_op(op);
            }
            depth
        })
}

fn contraction_candidates(
    context: &Context,
    round: &RoundOp,
) -> Result<Vec<ContractionProposal>, PassError> {
    const MAX_CANDIDATES: usize = 32;
    let candidates = contraction_proposals(context, round).unwrap_or_default();
    if candidates.len() > MAX_CANDIDATES {
        return Err(PassError::InvalidRuleSet(format!(
            "fp.round contraction search exhausted: found {} candidates, limit is {MAX_CANDIDATES}",
            candidates.len()
        )));
    }
    Ok(candidates)
}

fn materialize_candidate(
    context: &Context,
    round: &RoundOp,
    proposal: &ContractionProposal,
) -> Result<ContractionCandidate, PassError> {
    let destination = context
        .parent_nodes_region(round.handle().id)
        .ok_or(PassError::RewriteFailed(round.handle().id))?;
    let reference = round.reference_region();
    let bindings: HashMap<_, _> = reference
        .ports()
        .iter()
        .map(crate::Value::id)
        .zip(round.operands())
        .collect();
    let source_ops = crate::region::topological_order(context, reference.id())
        .unwrap_or_else(|_| reference.op_ids());
    let (copies, mut outputs) =
        crate::clone::clone_nodes_ops_into(context, reference.id(), &bindings, destination);
    let op_copies: HashMap<_, _> = source_ops
        .iter()
        .copied()
        .zip(copies.iter().copied())
        .collect();
    let mut values = bindings;
    for (source, copy) in source_ops.iter().zip(&copies) {
        for (old, new) in context
            .get_op(*source)
            .results()
            .iter()
            .zip(context.get_op(*copy).results())
        {
            values.insert(*old, new);
        }
    }
    let consumer_copy = op_copies[&proposal.add_or_sub];
    let mul_copy = op_copies[&proposal.mul];
    let consumer = context.get_op(consumer_copy);
    let retain_mul = proposal.retain_mul;
    let numeric_ty = context.get_value(consumer.value_results()[0]).ty();
    let mut addend = values[&proposal.fused_operands[2]];
    if proposal.form == FusedForm::MulSub {
        let neg = NegOpBuilder::new(context)
            .input(addend)
            .result_type(context.get_value(addend).ty())
            .build();
        context.add(destination, neg.id());
        addend = neg.result();
    }
    let fma = FmaOpBuilder::new(context)
        .a(values[&proposal.fused_operands[0]])
        .b(values[&proposal.fused_operands[1]])
        .c(addend)
        .semantics(context.intern_fp_semantics(round.contract().arithmetic))
        .state_operands(if retain_mul {
            consumer.state_operands()
        } else {
            context.get_op(mul_copy).state_operands()
        })
        .result_type(numeric_ty)
        .state_results(
            consumer
                .state_results()
                .iter()
                .map(|value| context.get_value(*value).ty()),
        )
        .build();
    context.add(destination, fma.id());
    let old_results = consumer.results();
    let new_results = fma.handle().results();
    for (old, new) in old_results.iter().zip(&new_results) {
        context.replace_value_uses(*old, *new);
        for output in &mut outputs {
            if *output == *old {
                *output = *new;
            }
        }
    }
    context.erase_op(&OperationRef::new(consumer))?;
    if !retain_mul {
        context.erase_op(&OperationRef::new(context.get_op(mul_copy)))?;
    }
    Ok(ContractionCandidate {
        mul: proposal.mul,
        add_or_sub: proposal.add_or_sub,
        implementation: fma.handle().id,
        fused_operands: proposal.fused_operands,
        form: proposal.form,
        result_mapping: reference.results().into_iter().zip(outputs).collect(),
    })
}

fn select_candidate(
    context: &Context,
    round: &RoundOp,
    candidate: &ContractionCandidate,
) -> Result<(), PassError> {
    let destination = context
        .parent_nodes_region(round.handle().id)
        .ok_or(PassError::RewriteFailed(round.handle().id))?;
    for (old, (_, new)) in round
        .handle()
        .results()
        .iter()
        .zip(&candidate.result_mapping)
    {
        let new = preserve_nested_round_boundary(context, round, destination, *new);
        context.replace_value_uses(*old, new);
        replace_region_result(context, round.handle().id, *old, new);
    }
    context.erase_op(&OperationRef::new(round.handle().clone()))
}

fn splice_reference(context: &Context, round: &RoundOp) -> Result<(), PassError> {
    let destination = context
        .parent_nodes_region(round.handle().id)
        .ok_or(PassError::RewriteFailed(round.handle().id))?;
    let reference = round.reference_region();
    let bindings: HashMap<_, _> = reference
        .ports()
        .iter()
        .map(crate::Value::id)
        .zip(round.operands())
        .collect();
    let (_, outputs) =
        crate::clone::clone_nodes_ops_into(context, reference.id(), &bindings, destination);
    for (old, new) in round.handle().results().iter().zip(outputs) {
        let new = preserve_nested_round_boundary(context, round, destination, new);
        context.replace_value_uses(*old, new);
        replace_region_result(context, round.handle().id, *old, new);
    }
    context.erase_op(&OperationRef::new(round.handle().clone()))
}

fn preserve_nested_round_boundary(
    context: &Context,
    round: &RoundOp,
    destination: crate::RegionId,
    value: crate::ValueId,
) -> crate::ValueId {
    let mut parent = context.parent_op(round.handle().id);
    while let Some(op) = parent {
        if context.get_op(op).is::<RoundOp>() {
            let ty = context.get_value(value).ty();
            let ty_data = context.get_type_data(ty);
            if (ty_data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<FloatType>()
                .is_some()
            {
                let fence = FenceOpBuilder::new(context)
                    .input(value)
                    .result_type(ty)
                    .build();
                context.add(destination, fence.id());
                return fence.result();
            }
            return value;
        }
        parent = context.parent_op(op);
    }
    value
}

fn replace_region_result(context: &Context, op: OpId, old: crate::ValueId, new: crate::ValueId) {
    let Some(region) = context.parent_nodes_region(op) else {
        return;
    };
    let mut results = context.get_region(region).results();
    let mut changed = false;
    for result in &mut results {
        if *result == old {
            *result = new;
            changed = true;
        }
    }
    if changed {
        context.set_region_results(region, results);
    }
}
