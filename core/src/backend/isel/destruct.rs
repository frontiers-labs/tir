//! The target's edges for destructuring a selected function.
//!
//! Once a function's regions hold machine instructions, the shared
//! destructuring turns its structure into blocks; what joins them is the
//! target's own branches. A test was selected with the region holding it and
//! arrives here as the [`AuxEmit`] the region's plan left: a branch rule fused
//! over the condition, the target's branch-if-nonzero over its register, or a
//! decision the region's facts already made. The values the edges carry were
//! selected too and arrive through the map of what selection left each value
//! as.

use std::collections::{HashMap, HashSet};

use tir::{BlockId, Context, OpHandle, OpId, PassError, ValueId};

use super::builder::AuxSlot;
use super::emit::{AuxEmit, GuardBranch};
use super::{BranchEmitters, Rule, RuleKind};
use crate::passes::destructure::{
    ControlDefinition, ControlId, DemandDomainId, Edge, Edges, RecoveryPlan, Test, ValueBinding,
};

pub(crate) struct SelectedControl {
    emit: AuxEmit,
    taken_when: bool,
}

/// Bind semantic control identities before recovery changes their consumers.
pub(crate) fn bind_controls(
    recovery: &RecoveryPlan,
    selected: &HashMap<(OpId, AuxSlot), AuxEmit>,
) -> Result<HashMap<(ControlId, usize), SelectedControl>, PassError> {
    let controls: HashMap<_, _> = selected
        .iter()
        .filter_map(|((_, slot), emit)| {
            let AuxSlot::Control {
                id,
                outcome,
                inverted,
            } = *slot
            else {
                return None;
            };
            Some((
                (id, outcome),
                SelectedControl {
                    emit: emit.clone(),
                    taken_when: !inverted,
                },
            ))
        })
        .collect();
    for definition in recovery.requirements() {
        for outcome in 0..definition.outcomes.len().saturating_sub(1) {
            if !controls.contains_key(&(definition.id, outcome)) {
                return Err(PassError::InvalidRuleSet(format!(
                    "control {:?} outcome {outcome} was not selected",
                    definition.id
                )));
            }
        }
    }
    Ok(controls)
}

fn selected_test(
    selected: &HashMap<(OpId, AuxSlot), AuxEmit>,
    consumer: OpId,
    test: Test,
) -> Result<(&AuxEmit, bool), PassError> {
    let index = match test {
        Test::Repeat => 0,
        Test::Arm(index) => index,
    };
    let (slot, taken_when) = if selected.contains_key(&(consumer, AuxSlot::Unless(index))) {
        (AuxSlot::Unless(index), false)
    } else {
        (AuxSlot::Test(index), true)
    };
    selected
        .get(&(consumer, slot))
        .map(|emit| (emit, taken_when))
        .ok_or_else(|| {
            PassError::InvalidRuleSet(format!(
                "control test {slot:?} of {consumer:?} was not selected"
            ))
        })
}

fn reads(emit: &AuxEmit) -> Vec<ValueId> {
    match emit {
        AuxEmit::Branch(GuardBranch::Fused { m, .. }) => m.values().collect(),
        AuxEmit::Branch(GuardBranch::Nonzero { condition }) => vec![*condition],
        AuxEmit::Decided(_) => Vec::new(),
    }
}

pub(crate) struct MachineEdges<'a> {
    pub(crate) context: &'a Context,
    pub(crate) emitters: &'a BranchEmitters,
    /// What selection left each IR value as.
    pub(crate) emitted: &'a HashMap<ValueId, ValueId>,
    pub(crate) region_values: &'a HashMap<(OpId, AuxSlot), AuxEmit>,
    pub(crate) controls: &'a HashMap<(ControlId, usize), SelectedControl>,
    pub(crate) domains: &'a HashMap<OpId, DemandDomainId>,
    pub(crate) literals: &'a HashSet<OpId>,
    /// The operations each instruction runs after besides those defining its
    /// operands: a rule's prelude, a call's tuple extractions.
    pub(crate) implicit: &'a HashMap<OpId, Vec<OpId>>,
    pub(crate) rules: &'a [Rule],
}

impl MachineEdges<'_> {
    fn value(&self, value: ValueId) -> ValueId {
        self.emitted.get(&value).copied().unwrap_or(value)
    }

    fn mapped(&self, values: &[ValueId]) -> Vec<ValueId> {
        values.iter().map(|&value| self.value(value)).collect()
    }

    fn selected(&self, op: &OpHandle, test: Test) -> Result<(&AuxEmit, bool), PassError> {
        selected_test(self.region_values, op.id, test)
    }

    fn emit_jump(&self, block: BlockId, dest: BlockId, args: &[ValueId]) {
        self.context
            .get_block(block)
            .append((self.emitters.uncond)(self.context, dest, args).id());
    }
    fn emit_selected(
        &self,
        selected: &SelectedControl,
        block: BlockId,
        taken: &Edge,
        fallthrough: &Edge,
        mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError> {
        let emit = &selected.emit;
        let holds = selected.taken_when;
        let (taken, fallthrough) = if holds {
            (taken, fallthrough)
        } else {
            (fallthrough, taken)
        };
        let taken_args = self.mapped(&taken.args);
        let fallthrough_args = self.mapped(&fallthrough.args);
        // A test the region's assumptions decided leaves one reachable edge.
        if let AuxEmit::Decided(holds) = emit {
            let (dest, args) = if *holds {
                (taken.dest, &taken_args)
            } else {
                (fallthrough.dest, &fallthrough_args)
            };
            self.emit_jump(block, dest, args);
            return Ok(());
        }
        // A taken edge has no operand slot to carry assignments in, so one that
        // performs any goes through a block only it reaches. A dependency
        // argument is not an assignment — nothing moves for it, and the
        // parameter names the chain the join is entered on — so an edge
        // carrying only those needs no block of its own.
        let deps = self.context.get_block(taken.dest).state_arguments().len();
        let target = if taken_args.len() > deps {
            let trampoline = mint();
            self.emit_jump(trampoline, taken.dest, &taken_args);
            trampoline
        } else {
            taken.dest
        };
        let holder = self.context.get_block(block);
        let AuxEmit::Branch(branch) = emit else {
            unreachable!("a decided test was taken above");
        };
        match branch {
            GuardBranch::Nonzero { condition } => {
                for op in (self.emitters.cond_nonzero)(self.context, *condition, target) {
                    holder.append(op.id());
                }
            }
            GuardBranch::Fused { rule_index, m } => {
                let rule = &self.rules[*rule_index];
                let RuleKind::CondBranch { target_symbol } = rule.kind else {
                    return Err(PassError::InvalidRuleSet(
                        "a fused gate test is not a conditional branch".to_string(),
                    ));
                };
                let mut m = m.clone();
                m.rebind_block(target_symbol, target);
                let request = super::EmitRequest {
                    op: None,
                    results: &[],
                    result_ty: None,
                    states: &[],
                };
                if let Some(prelude) = rule.prelude_emit {
                    holder.append(prelude(self.context, &request, &m)?.id());
                }
                holder.append((rule.emit_fn)(self.context, &request, &m)?.id());
            }
        }
        self.emit_jump(block, fallthrough.dest, &fallthrough_args);
        Ok(())
    }
}

impl Edges for MachineEdges<'_> {
    fn jump(&self, block: BlockId, edge: &Edge) {
        self.emit_jump(block, edge.dest, &self.mapped(&edge.args));
    }

    /// Branch on a selected test: the taken edge goes through a block only it
    /// reaches where it carries assignments, and the untaken one falls through
    /// carrying its own.
    fn branch(
        &self,
        block: BlockId,
        op: &OpHandle,
        test: Test,
        taken: &Edge,
        fallthrough: &Edge,
        mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError> {
        let (emit, taken_when) = self.selected(op, test)?;
        let selected = SelectedControl {
            emit: emit.clone(),
            taken_when,
        };
        self.emit_selected(&selected, block, taken, fallthrough, mint)
    }

    fn branch_control(
        &self,
        control: &ControlDefinition,
        outcome: usize,
        _predicate: ValueId,
        bindings: &[ValueBinding],
        block: BlockId,
        taken: &Edge,
        fallthrough: &Edge,
        mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError> {
        let selected = self.controls.get(&(control.id, outcome)).ok_or_else(|| {
            PassError::InvalidRuleSet(format!("control {:?} has no selected branch", control.id))
        })?;
        let substitutions: HashMap<ValueId, ValueId> = bindings
            .iter()
            .map(|binding| (self.value(binding.source), self.value(binding.current)))
            .collect();
        let mut emit = selected.emit.clone();
        match &mut emit {
            AuxEmit::Branch(GuardBranch::Fused { m, .. }) => m.remap_values(&substitutions),
            AuxEmit::Branch(GuardBranch::Nonzero { condition }) => {
                *condition = substitutions.get(condition).copied().unwrap_or(*condition);
            }
            AuxEmit::Decided(_) => {}
        }
        let selected = SelectedControl {
            emit,
            taken_when: selected.taken_when,
        };
        self.emit_selected(&selected, block, taken, fallthrough, mint)
    }

    fn control_reads(&self, control: &ControlDefinition, outcome: usize) -> Vec<ValueId> {
        self.controls
            .get(&(control.id, outcome))
            .map_or_else(Vec::new, |selected| reads(&selected.emit))
    }

    fn decided_control(&self, control: &ControlDefinition, outcome: usize) -> Option<bool> {
        let selected = self.controls.get(&(control.id, outcome))?;
        match &selected.emit {
            AuxEmit::Decided(holds) => Some(*holds == selected.taken_when),
            _ => None,
        }
    }

    fn value(&self, source: ValueId) -> ValueId {
        MachineEdges::value(self, source)
    }

    fn compatible_type(&self, source: tir::TypeId, selected: tir::TypeId) -> bool {
        source == selected
            || (!self.context.is_state_type(source)
                && (self.context.get_type_data(selected).as_ref() as &dyn std::any::Any)
                    .is::<crate::backend::registers::RegClassType>())
    }

    fn is_literal(&self, op: OpId) -> bool {
        self.literals.contains(&op)
    }

    fn execution_domain(&self, op: OpId) -> Option<DemandDomainId> {
        self.domains.get(&op).copied()
    }

    fn decided(&self, op: &OpHandle, test: Test) -> Option<bool> {
        match self.selected(op, test) {
            Ok((AuxEmit::Decided(holds), taken_when)) => Some(*holds == taken_when),
            _ => None,
        }
    }

    fn test_reads(&self, op: &OpHandle, test: Test) -> Vec<ValueId> {
        self.selected(op, test)
            .map_or_else(|_| Vec::new(), |(emit, _)| reads(emit))
    }

    fn implicit_inputs(&self, op: OpId) -> Vec<OpId> {
        self.implicit.get(&op).cloned().unwrap_or_default()
    }

    /// The function's return is not a machine instruction yet: the target's
    /// lowering of the function turns it into one once the blocks exist.
    fn leave(&self, block: BlockId, values: &[ValueId], deps: &[ValueId]) -> Result<(), PassError> {
        let mut builder = tir::func::ReturnOpBuilder::new(self.context);
        if let Some(&value) = values.first() {
            builder = builder.value(self.value(value));
        }
        for &dep in deps {
            builder = builder.state(self.value(dep));
        }
        self.context
            .get_block(block)
            .append(tir::Operation::id(&builder.build()));
        Ok(())
    }
}
