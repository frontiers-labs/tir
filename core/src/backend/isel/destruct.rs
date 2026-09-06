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

use std::collections::HashMap;

use tir::{BlockId, Context, OpHandle, OpId, PassError, ValueId};

use super::builder::AuxSlot;
use super::emit::{AuxEmit, GuardBranch};
use super::{BranchEmitters, Rule, RuleKind};
use crate::passes::destructure::{Edge, Edges, Test};

pub(crate) struct MachineEdges<'a> {
    pub(crate) context: &'a Context,
    pub(crate) emitters: &'a BranchEmitters,
    /// What selection left each IR value as.
    pub(crate) emitted: &'a HashMap<ValueId, ValueId>,
    pub(crate) region_values: &'a HashMap<(OpId, AuxSlot), AuxEmit>,
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

    fn decline(op: &OpHandle, reason: &str) -> PassError {
        PassError::InvalidRuleSet(format!("cannot destructure {}: {reason}", op.name()))
    }

    /// The selected test for `test` of `op`, and whether the taken edge is the
    /// one the test holds on.
    fn selected(&self, op: &OpHandle, test: Test) -> Result<(&AuxEmit, bool), PassError> {
        let (slot, holds) = match test {
            Test::Repeat => (AuxSlot::Test(0), true),
            Test::Arm(index) => {
                if self.region_values.contains_key(&(op.id, AuxSlot::Unless(index))) {
                    (AuxSlot::Unless(index), false)
                } else {
                    (AuxSlot::Test(index), true)
                }
            }
        };
        self.region_values
            .get(&(op.id, slot))
            .map(|emit| (emit, holds))
            .ok_or_else(|| Self::decline(op, &format!("{slot:?} was not selected")))
    }

    fn emit_jump(&self, block: BlockId, dest: BlockId, args: &[ValueId]) {
        self.context
            .get_block(block)
            .append((self.emitters.uncond)(self.context, dest, args).id());
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
        let (emit, holds) = self.selected(op, test)?;
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
        let deps = self.context.get_block(taken.dest).dep_arguments().len();
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
                    state: None,
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

    fn decided(&self, op: &OpHandle, test: Test) -> Option<bool> {
        match self.selected(op, test) {
            Ok((AuxEmit::Decided(holds), taken_when)) => Some(*holds == taken_when),
            _ => None,
        }
    }

    fn test_reads(&self, op: &OpHandle, test: Test) -> Vec<ValueId> {
        match self.selected(op, test) {
            Ok((AuxEmit::Branch(GuardBranch::Fused { m, .. }), _)) => m.values().collect(),
            Ok((AuxEmit::Branch(GuardBranch::Nonzero { condition }), _)) => vec![*condition],
            _ => Vec::new(),
        }
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
            builder = builder.dep_operand(self.value(dep));
        }
        self.context
            .get_block(block)
            .append(tir::Operation::id(&builder.build()));
        Ok(())
    }
}
