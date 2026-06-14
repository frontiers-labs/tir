//! Emission planning: turning a solved cover into per-op decisions and the
//! instructions materialized for rewrite-introduced e-classes.

use std::collections::HashMap;

use tir::{Context, OpId, TypeId, ValueId, builtin::IntegerType, egraph::EClassId};

use super::RuleMatch;
use super::cover::PbqpIselMatch;
use super::node::{Binding, SemEGraph, class_binding, class_width};

#[derive(Clone, Debug)]
pub(crate) enum BlockDecision {
    Emit { rule_index: usize, m: RuleMatch },
    Consume,
}
/// The emission plan for a block: how each original op is rewritten, plus the extra
/// instructions to insert for rewrite-introduced e-classes that have no original op
/// (the `slli` of a `slli`/`srai` sign-extension expansion).
#[derive(Clone, Debug, Default)]
pub(crate) struct BlockPlan {
    pub(crate) op_decisions: HashMap<OpId, BlockDecision>,
    pub(crate) introduced: Vec<IntroducedEmit>,
    /// Definer instructions to insert ahead of an op for the implicit register
    /// uses in its behavior (e.g. `vsetvli` defining `VCSR::vl` that `vadd` reads).
    pub(crate) definers: Vec<DefinerEmit>,
}

/// A definer instruction to materialize just before `anchor`: it defines a
/// register the anchor implicitly reads, consuming the value that read bound to.
/// It has no SSA destination of its own (its emitter hardwires one, e.g. `x0`).
#[derive(Clone, Debug)]
pub(crate) struct DefinerEmit {
    pub(crate) definer_index: usize,
    pub(crate) m: RuleMatch,
    pub(crate) anchor: OpId,
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
    pub(crate) egraph: &'a SemEGraph,
    pub(crate) class_value: &'a HashMap<EClassId, ValueId>,
    pub(crate) op_by_root: &'a HashMap<EClassId, OpId>,
    pub(crate) matches: &'a [PbqpIselMatch],
    pub(crate) root_match: &'a HashMap<EClassId, usize>,
    pub(crate) context: &'a Context,
    /// Fresh destination value assigned to each introduced class.
    pub(crate) introduced_dest: HashMap<EClassId, ValueId>,
    pub(crate) introduced: Vec<IntroducedEmit>,
}

impl EmissionBuilder<'_> {
    /// A Root-covered class with no original op is one the rewrites introduced.
    fn is_introduced(&self, class: EClassId) -> bool {
        self.root_match.contains_key(&class) && !self.op_by_root.contains_key(&class)
    }

    /// Build the operand bindings for a match, first materializing any introduced
    /// operand instructions (anchored before `anchor`).
    pub(crate) fn resolve_match(
        &mut self,
        match_id: usize,
        anchor: OpId,
        anchor_ty: Option<TypeId>,
    ) -> RuleMatch {
        let operand_classes: Vec<EClassId> = self.matches[match_id]
            .bindings
            .captures
            .entries
            .iter()
            .map(|(_, class)| self.egraph.find(*class))
            .collect();
        for class in operand_classes {
            if self.is_introduced(class) {
                self.emit_introduced(class, anchor, anchor_ty);
            }
        }
        self.build_rule_match(match_id)
    }

    /// Ensure an introduced class is emitted (operands first), returning its fresh
    /// destination value.
    fn emit_introduced(
        &mut self,
        class: EClassId,
        anchor: OpId,
        anchor_ty: Option<TypeId>,
    ) -> ValueId {
        if let Some(&dest) = self.introduced_dest.get(&class) {
            return dest;
        }
        let match_id = self.root_match[&class];
        let operand_classes: Vec<EClassId> = self.matches[match_id]
            .bindings
            .captures
            .entries
            .iter()
            .map(|(_, c)| self.egraph.find(*c))
            .collect();
        for c in operand_classes {
            if self.is_introduced(c) {
                self.emit_introduced(c, anchor, anchor_ty);
            }
        }

        let dest_ty = anchor_ty
            .or_else(|| {
                class_width(self.context, self.egraph, class)
                    .map(|w| IntegerType::new(self.context, w))
            })
            .unwrap_or_else(|| IntegerType::new(self.context, 64));
        let dest = self.context.create_value(dest_ty, None).id();
        self.introduced_dest.insert(class, dest);

        let m = self.build_rule_match(match_id);
        self.introduced.push(IntroducedEmit {
            rule_index: self.matches[match_id].rule_index,
            m,
            dest,
            dest_ty,
            anchor,
        });
        dest
    }

    /// Resolve each capture symbol to a concrete operand: an introduced operand's
    /// fresh value, then a constant immediate, then an input value, then the value
    /// an intermediate result produces.
    fn build_rule_match(&self, match_id: usize) -> RuleMatch {
        let mut int_bindings = Vec::new();
        let mut value_bindings = Vec::new();
        for (sym, class) in &self.matches[match_id].bindings.captures.entries {
            let class = self.egraph.find(*class);
            // An introduced operand's fresh value takes priority; otherwise resolve
            // the class to its constant/input/intermediate operand as usual.
            if let Some(&dest) = self.introduced_dest.get(&class) {
                value_bindings.push((*sym, dest));
                continue;
            }
            match class_binding(self.egraph, self.class_value, class) {
                Some(Binding::Int(v)) => int_bindings.push((*sym, v)),
                Some(Binding::Value(v)) => value_bindings.push((*sym, v)),
                None => {}
            }
        }
        RuleMatch::new(int_bindings, value_bindings)
    }
}
