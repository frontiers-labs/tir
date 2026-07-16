//! Emission planning: turning a solved cover into per-op decisions and the
//! instructions materialized for rewrite-introduced e-classes.

use std::collections::HashMap;

use tir::{BlockId, Context, OpId, TypeId, ValueId, analysis::DominatorTree, builtin::IntegerType};
use tir_symbolic::egraph::Id;

use super::FunctionSelection;
use super::RuleMatch;
use super::cover::PbqpIselMatch;
use super::node::{class_int_binding, class_width};

#[derive(Clone, Debug)]
pub(crate) enum BlockDecision {
    Emit {
        rule_index: usize,
        m: RuleMatch,
    },
    Consume,
    /// A low-bit truncation: forward the operand's register to the result (a
    /// re-view, not a computation) and erase the op. Consumers read the operand,
    /// which is resolved from the op's live operand list at commit time (not
    /// captured at plan time) so a cross-block truncation chain chases the
    /// already-remapped value regardless of block commit order.
    ForwardOperand,
}
/// The emission plan for a block: how each original op is rewritten, plus the extra
/// instructions to insert for rewrite-introduced e-classes that have no original op
/// (the `slli` of a `slli`/`srai` sign-extension expansion).
#[derive(Clone, Debug, Default)]
pub(crate) struct BlockPlan {
    pub(crate) op_decisions: HashMap<OpId, BlockDecision>,
    pub(crate) introduced: Vec<IntroducedEmit>,
    /// How each control-flow terminator is lowered (only when the target
    /// installed branch emitters).
    pub(crate) terminators: Vec<TerminatorPlan>,
}

/// The lowering of one terminator op.
#[derive(Clone, Debug)]
pub(crate) enum TerminatorPlan {
    /// A guarded two-way terminator: insert the conditional branch to the true
    /// successor ahead of `op`, then replace `op` with an unconditional branch
    /// to `false_dest`.
    Guard {
        op: OpId,
        branch: GuardBranch,
        false_dest: BlockId,
    },
    /// An unconditional branch, lowered through the target's `uncond` emitter.
    Jump {
        op: OpId,
        dest: BlockId,
        args: Vec<ValueId>,
    },
}

/// The conditional branch chosen for a guard: a fused branch rule recomputing
/// the condition from its operands, or the target's branch-if-nonzero fallback
/// reading the materialized condition register.
#[derive(Clone, Debug)]
pub(crate) enum GuardBranch {
    Fused { rule_index: usize, m: RuleMatch },
    Nonzero { condition: ValueId, dest: BlockId },
}

/// An instruction to materialize for an introduced e-class: emitted with a fresh
/// destination value and inserted just before `anchor` (the source op whose
/// expansion produced it). Operands precede consumers in `BlockPlan::introduced`.
#[derive(Clone, Debug)]
pub(crate) struct IntroducedEmit {
    pub(crate) rule_index: usize,
    pub(crate) m: RuleMatch,
    pub(crate) dest: ValueId,
    pub(crate) dest_ty: TypeId,
    pub(crate) anchor: OpId,
}
/// Turns a solved cover into concrete per-instruction `RuleMatch`es, materializing
/// rewrite-introduced e-classes (those covered by a Root match but with no original
/// IR op) as fresh-valued instructions threaded into their consumers' operands.
pub(crate) struct EmissionBuilder<'a> {
    pub(crate) fs: &'a FunctionSelection,
    pub(crate) dom: &'a DominatorTree,
    /// The block whose plan is being emitted (the consumer's block).
    pub(crate) block: BlockId,
    pub(crate) matches: &'a [PbqpIselMatch],
    pub(crate) root_match: &'a HashMap<Id, usize>,
    pub(crate) context: &'a Context,
    /// Fresh destination value assigned to each introduced class.
    pub(crate) introduced_dest: HashMap<Id, ValueId>,
    pub(crate) introduced: Vec<IntroducedEmit>,
}

impl EmissionBuilder<'_> {
    /// A Root-covered class with no original op is one the rewrites introduced.
    /// The op-root test aggregates over the scoped class's base members, so a
    /// fact-merged op class is not mistaken for an introduced one.
    fn is_introduced(&self, class: Id) -> bool {
        self.root_match.contains_key(&class) && !self.fs.is_op_root(class)
    }

    /// Build the operand bindings for a match, first materializing any introduced
    /// operand instructions (anchored before `anchor`).
    pub(crate) fn resolve_match(
        &mut self,
        match_id: usize,
        anchor: OpId,
        anchor_ty: Option<TypeId>,
    ) -> RuleMatch {
        let operand_classes: Vec<Id> = self.matches[match_id]
            .bindings
            .captures
            .entries
            .iter()
            .map(|(_, class)| self.fs.egraph.find(*class))
            .collect();
        for class in operand_classes {
            if self.is_introduced(class) {
                self.emit_introduced(class, anchor, anchor_ty);
            }
        }
        self.build_rule_match(match_id, anchor)
    }

    /// Ensure an introduced class is emitted (operands first), returning its fresh
    /// destination value.
    fn emit_introduced(&mut self, class: Id, anchor: OpId, anchor_ty: Option<TypeId>) -> ValueId {
        if let Some(&dest) = self.introduced_dest.get(&class) {
            return dest;
        }
        let match_id = self.root_match[&class];
        let operand_classes: Vec<Id> = self.matches[match_id]
            .bindings
            .captures
            .entries
            .iter()
            .map(|(_, c)| self.fs.egraph.find(*c))
            .collect();
        for c in operand_classes {
            // A materializer's immediate operand binds the rooted class itself
            // (the injected `Add(zext 0, C)` li form is a member of `C`): not a
            // real operand dependency, so it must not recurse.
            if c != class && self.is_introduced(c) {
                self.emit_introduced(c, anchor, anchor_ty);
            }
        }

        let dest_ty = class_width(self.context, &self.fs.egraph, class)
            .map(|w| IntegerType::new(self.context, w))
            .or(anchor_ty)
            .unwrap_or_else(|| IntegerType::new(self.context, 64));
        let dest = self.context.create_value(dest_ty, None).id();
        self.introduced_dest.insert(class, dest);

        let m = self.build_rule_match(match_id, anchor);
        self.introduced.push(IntroducedEmit {
            rule_index: self.matches[match_id].rule_index,
            m,
            dest,
            dest_ty,
            anchor,
        });
        dest
    }

    /// Resolve each capture symbol to a concrete operand for consumer op
    /// `consumer`: an introduced operand's fresh value, then — through the one
    /// shared resolver — a constant immediate and/or a register value under the
    /// dominance rule (a class can carry both).
    fn build_rule_match(&self, match_id: usize, consumer: OpId) -> RuleMatch {
        let mut int_bindings = Vec::new();
        let mut value_bindings = Vec::new();
        let root = self.fs.egraph.find(self.matches[match_id].root);
        for (sym, class) in &self.matches[match_id].bindings.captures.entries {
            let class = self.fs.egraph.find(*class);
            // A self-referential operand (a materializer's immediate binds its
            // own rooted class) resolves to the class's constant, not to the
            // instruction's own not-yet-written destination.
            if class != root
                && let Some(&dest) = self.introduced_dest.get(&class)
            {
                value_bindings.push((*sym, dest));
                if let Some(value) = class_int_binding(&self.fs.egraph, class) {
                    int_bindings.push((*sym, value));
                }
                continue;
            }
            let binding =
                self.fs
                    .resolve_binding(self.dom, self.context, class, self.block, consumer);
            if let Some(v) = binding.int {
                int_bindings.push((*sym, v));
            }
            if let Some(v) = binding.value {
                value_bindings.push((*sym, v));
            }
        }
        RuleMatch::new(int_bindings, value_bindings)
    }
}
