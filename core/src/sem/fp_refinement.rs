use std::hash::{Hash, Hasher};

use crate::fp::ops::{AddOp, FmaOp, MulOp, RoundOp, SubOp};
use crate::fp::{Exceptions, IntermediateExceptions};
use crate::{Context, OpId, ValueId};

mod graph;

use graph::ContractSnapshot;
pub use graph::{CandidateSnapshot, ReferenceSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FusedForm {
    MulAdd,
    MulSub,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContractionCandidate {
    pub mul: OpId,
    pub add_or_sub: OpId,
    pub implementation: OpId,
    pub fused_operands: [ValueId; 3],
    pub form: FusedForm,
    pub result_mapping: Vec<(ValueId, ValueId)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContractionProposal {
    pub mul: OpId,
    pub add_or_sub: OpId,
    pub fused_operands: [ValueId; 3],
    pub form: FusedForm,
    pub retain_mul: bool,
}

pub fn contraction_proposals(
    context: &Context,
    round: &RoundOp,
) -> Result<Vec<ContractionProposal>, RefinementError> {
    graph::contraction_proposals(context, round)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefinementError {
    StaleOperation,
    OutsideReference,
    DisconnectedGroup,
    IncompatibleSemantics,
    ContractForbidsContraction,
    IntermediateExceptionsObserved,
    IncompleteResultMapping,
    UnsupportedAccuracy,
    UnsupportedForm,
}

pub fn check_contraction_rule(rule: &tir_pdl::Rule) -> Result<(), RefinementError> {
    use tir_pdl::{ContractPermission, RefinementRequirement, RuleKind};

    if !matches!(rule.kind, RuleKind::Refinement) {
        return Err(RefinementError::UnsupportedForm);
    }
    let required = [
        RefinementRequirement::SameRoundRegion,
        RefinementRequirement::Permits(ContractPermission::ContractMulAdd),
        RefinementRequirement::CompatibleFormatsAndRounding,
        RefinementRequirement::PermittedEffectChange,
        RefinementRequirement::SatisfiesDomainAndEffectContract,
    ];
    if required
        .iter()
        .any(|requirement| !rule.requirements.contains(requirement))
    {
        return Err(RefinementError::ContractForbidsContraction);
    }
    graph::check_contraction_rule(rule)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefinementWitness {
    fingerprint: u64,
    contract: ContractSnapshot,
    reference: ReferenceSnapshot,
    candidate: CandidateSnapshot,
    site: graph::ContractionSite,
    form: FusedForm,
}

impl RefinementWitness {
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    pub fn validate(
        &self,
        context: &Context,
        round: &RoundOp,
        candidate: &ContractionCandidate,
    ) -> Result<(), RefinementError> {
        let current = check_contraction(context, round, candidate)?;
        (current.fingerprint == self.fingerprint
            && current.contract == self.contract
            && current.reference == self.reference
            && current.candidate == self.candidate
            && current.site == self.site
            && current.form == self.form)
            .then_some(())
            .ok_or(RefinementError::StaleOperation)
    }
}

pub fn check_contraction(
    context: &Context,
    round: &RoundOp,
    candidate: &ContractionCandidate,
) -> Result<RefinementWitness, RefinementError> {
    if !round.contract().permissions.expression_contraction {
        return Err(RefinementError::ContractForbidsContraction);
    }
    if !matches!(
        round.contract().accuracy,
        crate::fp::AccuracyRequirement::Reference
    ) {
        return Err(RefinementError::UnsupportedAccuracy);
    }
    if !context.has_operation(candidate.mul)
        || !context.has_operation(candidate.add_or_sub)
        || !context.has_operation(candidate.implementation)
    {
        return Err(RefinementError::StaleOperation);
    }
    contraction_proposals(context, round)?
        .into_iter()
        .find(|proposal| {
            proposal.mul == candidate.mul
                && proposal.add_or_sub == candidate.add_or_sub
                && proposal.fused_operands == candidate.fused_operands
                && proposal.form == candidate.form
        })
        .ok_or(RefinementError::DisconnectedGroup)?;
    let mul_handle = context.get_op(candidate.mul);
    let consumer_handle = context.get_op(candidate.add_or_sub);
    let implementation_handle = context.get_op(candidate.implementation);
    if !mul_handle.is_live() || !consumer_handle.is_live() || !implementation_handle.is_live() {
        return Err(RefinementError::StaleOperation);
    }
    let mul = mul_handle
        .clone()
        .as_op::<MulOp>()
        .ok_or(RefinementError::UnsupportedForm)?;
    let implementation = implementation_handle
        .clone()
        .as_op::<FmaOp>()
        .ok_or(RefinementError::UnsupportedForm)?;
    if !direct_state_chain(context, &mul_handle, &consumer_handle) {
        return Err(RefinementError::DisconnectedGroup);
    }
    let reference = round.reference_region().id();
    for handle in [&mul_handle, &consumer_handle] {
        if context.parent_nodes_region(handle.id) != Some(reference) {
            return Err(RefinementError::OutsideReference);
        }
    }
    let mul_semantics_attr = mul.semantics();
    let mul_semantics = mul_semantics_attr
        .arithmetic()
        .map_err(|_| RefinementError::IncompatibleSemantics)?;
    let consumer_semantics = if let Some(add) = consumer_handle.clone().as_op::<AddOp>() {
        *add.semantics()
            .arithmetic()
            .map_err(|_| RefinementError::IncompatibleSemantics)?
    } else {
        *consumer_handle
            .clone()
            .as_op::<SubOp>()
            .ok_or(RefinementError::UnsupportedForm)?
            .semantics()
            .arithmetic()
            .map_err(|_| RefinementError::IncompatibleSemantics)?
    };
    let implementation_semantics = *implementation
        .semantics()
        .arithmetic()
        .map_err(|_| RefinementError::IncompatibleSemantics)?;
    if *mul_semantics != round.contract().arithmetic
        || consumer_semantics != round.contract().arithmetic
        || implementation_semantics != round.contract().arithmetic
    {
        return Err(RefinementError::IncompatibleSemantics);
    }
    if round.contract().arithmetic.exceptions != Exceptions::Ignore
        && round.contract().intermediate_exceptions
            != IntermediateExceptions::AllowContractedGroupRemoval
    {
        return Err(RefinementError::IntermediateExceptionsObserved);
    }
    let yielded = round.reference_region().results();
    if candidate.result_mapping.len() != yielded.len()
        || candidate
            .result_mapping
            .iter()
            .enumerate()
            .any(|(index, (source, candidate_value))| {
                yielded.get(index).copied() != Some(*source)
                    || consumer_handle
                        .results()
                        .iter()
                        .position(|result| result == source)
                        .is_some_and(|result| {
                            implementation_handle.results().get(result).copied()
                                != Some(*candidate_value)
                        })
            })
    {
        return Err(RefinementError::IncompleteResultMapping);
    }
    let contract = ContractSnapshot::capture(context, &round.contract())?;
    let (reference, site) = ReferenceSnapshot::capture_contraction(context, round, candidate)?;
    let candidate_snapshot = CandidateSnapshot::capture(context, round, candidate)?;
    if !candidate_snapshot.is_contraction_of(&reference, site) {
        return Err(RefinementError::DisconnectedGroup);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contract.hash(&mut hasher);
    reference.hash(&mut hasher);
    candidate_snapshot.hash(&mut hasher);
    site.hash(&mut hasher);
    candidate.form.hash(&mut hasher);
    Ok(RefinementWitness {
        fingerprint: hasher.finish(),
        contract,
        reference,
        candidate: candidate_snapshot,
        site,
        form: candidate.form,
    })
}

fn direct_state_chain(
    context: &Context,
    mul: &crate::OpHandle,
    consumer: &crate::OpHandle,
) -> bool {
    let produced = mul.state_results();
    let consumed = consumer.state_operands();
    produced.len() == consumed.len()
        && produced.iter().all(|output| {
            let resource = context.state_resource(context.get_value(*output).ty());
            resource.is_some()
                && consumed.iter().any(|input| {
                    *input == *output
                        && context.state_resource(context.get_value(*input).ty()) == resource
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Operation;
    use crate::builtin::{FloatType, ModuleOp};

    const ROUND_MODULE: &str = r#"
#contract = {accuracy = {kind = "reference"}, arithmetic = {exceptions = "ignore", kind = "arithmetic", nan = "any_quiet", rounding = "nearest_even", subnormals = "gradual", tininess = "after_rounding"}, assumptions = {finite = false}, environment_epoch = 0, intermediate_exceptions = "allow_contracted_group_removal", permissions = {approved_approximation = false, cross_statement_contraction = false, expression_contraction = true, ignore_signed_zero = false, reassociation = false, reciprocal = false}, result_formats = [!f64]}
module {
  %f = func.func @contract(%a: !f64, %b: !f64, %c: !f64) -> !f64 {
    %result = fp.round (%x = %a, %y = %a, %z = %c) {contract = #contract} : !f64 {
      %product = fp.mul %x, %y : !f64
      %sum = fp.add %product, %z : !f64
      -> %sum
    }
    %candidate = fp.fma %a, %a, %c : !f64
    -> %result
  }
  module_end
}
"#;

    #[test]
    fn reference_snapshot_is_independent_of_context_ids() {
        let (first, first_module) = parsed_module(false);
        let first_round = find_round(&first, first_module.id());
        let first_candidate = candidate(&first, first_module.id(), &first_round);

        let (second, second_module) = parsed_module(true);
        let second_round = find_round(&second, second_module.id());
        let second_candidate = candidate(&second, second_module.id(), &second_round);

        assert_eq!(
            ReferenceSnapshot::capture(&first, &first_round).unwrap(),
            ReferenceSnapshot::capture(&second, &second_round).unwrap()
        );
        assert_eq!(
            CandidateSnapshot::capture(&first, &first_round, &first_candidate).unwrap(),
            CandidateSnapshot::capture(&second, &second_round, &second_candidate).unwrap()
        );
    }

    #[test]
    fn witness_validates_unchanged_candidate() {
        let (context, module) = parsed_module(false);
        let round = find_round(&context, module.id());
        let candidate = candidate(&context, module.id(), &round);
        let witness = check_contraction(&context, &round, &candidate).unwrap();
        witness.validate(&context, &round, &candidate).unwrap();
    }

    #[test]
    fn witness_rejects_changed_output_mapping() {
        let (context, module) = parsed_module(false);
        let round = find_round(&context, module.id());
        let candidate = candidate(&context, module.id(), &round);
        let witness = check_contraction(&context, &round, &candidate).unwrap();
        let mut changed_output = candidate;
        changed_output.result_mapping[0].1 = round.handle().results()[0];
        assert!(witness.validate(&context, &round, &changed_output).is_err());
    }

    fn parsed_module(offset_ids: bool) -> (Context, ModuleOp) {
        let context = Context::with_default_dialects();
        if offset_ids {
            FloatType::f32(&context);
        }
        let module = crate::parse::ir::parse_ir::<ModuleOp>(&context, ROUND_MODULE).unwrap();
        (context, module)
    }

    fn candidate(context: &Context, root: OpId, round: &RoundOp) -> ContractionCandidate {
        let mul = find_op::<MulOp>(context, root);
        let add = find_op::<AddOp>(context, root);
        let fma = find_op::<FmaOp>(context, root);
        ContractionCandidate {
            mul,
            add_or_sub: add,
            implementation: fma,
            fused_operands: [
                round.reference_region().ports()[0].id(),
                round.reference_region().ports()[1].id(),
                round.reference_region().ports()[2].id(),
            ],
            form: FusedForm::MulAdd,
            result_mapping: vec![(
                round.reference_region().results()[0],
                context.get_op(fma).results()[0],
            )],
        }
    }

    fn find_op<T: Operation>(context: &Context, root: OpId) -> OpId {
        find_op_under::<T>(context, root).expect("module contains requested operation")
    }

    fn find_op_under<T: Operation>(context: &Context, root: OpId) -> Option<OpId> {
        let operation = context.get_op(root);
        if operation.is::<T>() {
            return Some(root);
        }
        operation
            .regions()
            .iter()
            .flat_map(|region| context.get_region(*region).op_ids())
            .find_map(|child| find_op_under::<T>(context, child))
    }

    fn find_round(context: &Context, root: OpId) -> RoundOp {
        find_round_under(context, root).expect("module contains fp.round")
    }

    fn find_round_under(context: &Context, root: OpId) -> Option<RoundOp> {
        let operation = context.get_op(root);
        if let Some(round) = operation.clone().as_op::<RoundOp>() {
            return Some(round);
        }
        operation
            .regions()
            .iter()
            .flat_map(|region| context.get_region(*region).op_ids())
            .find_map(|child| find_round_under(context, child))
    }
}
