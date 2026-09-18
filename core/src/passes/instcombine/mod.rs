//! InstCombine: an equality-saturation simplifier. It seeds the function's
//! regions ([`seed`], which reads gates off the ops' own interfaces) into a
//! [`tir_relational`] e-graph of real IR values, saturates, extracts the cheapest
//! form per value by [`crate::OpCost`], and rewrites what improved.
//!
//! Flow-sensitive facts ride the e-graph's scoped assumptions, both ways round. A
//! structured region pushes its guard's condition around its body and pops it on
//! the way back out; a loop's carried port is *hypothesised* to hold the constant
//! the loop was entered on, and what no edge back into it refutes is promoted
//! into the base graph. The region tree is the dominance, so nothing here
//! computes one.
//!
//! What a rewrite leaves behind is the same commit's business: the operation it
//! took the readers of, and every operation only that one read, are erased on the
//! way out.
//!
//! The engine holds no op-specific knowledge — identity, cost, folding and
//! constant-reading come from op interfaces; op construction is owned by the rewrites.

mod nodes;
pub(crate) mod rules;
mod seed;
mod state;

pub use nodes::InstCombineNodesPass;

use seed::{LoopPorts, Port};

use std::collections::HashMap;

use tir_relational::{ClassId as Id, Engine};

use crate::{
    Context, OpId, ValueId,
    attributes::{AttributeValue, Predicate},
    builtin::ops,
    utils::APInt,
};

use crate::sem::node::cost;
use crate::sem::{Prov, SemNode as Node, SymKind};
use rules::{Ruleset, builtin_ruleset};

const ITER_LIMIT: usize = 30;
const NODE_LIMIT: usize = 100_000;

/// Rewrites each region under the assumptions that hold there, and *before* its
/// children's scopes open so the base classes a child scope reads are final.
struct Driver<'a> {
    context: &'a Context,
    eg: Engine<Node>,
    value_class: HashMap<ValueId, Id>,
    ruleset: Ruleset,
    /// The value whose readers are being rewired. It answers for its own class
    /// and would answer every rewrite with itself, so no spelling may pick it.
    replacing: std::cell::Cell<Option<ValueId>>,
}

impl Driver<'_> {
    /// Saturate under whatever assumptions are open.
    fn saturate(&mut self) {
        self.eg.saturate_rules(
            &self.ruleset.rewrites,
            &self.ruleset.interpretation,
            ITER_LIMIT,
            NODE_LIMIT,
        );
        crate::memstats::egraph_census("instcombine", &self.eg);
    }

    /// Prove the loop-carried values a loop never changes, optimistically:
    /// hypothesise that a port holds the value the loop was entered on, run the
    /// body under that hypothesis, and union into the base graph the ports no
    /// edge back into them refutes. What does not survive is dropped and the
    /// round runs again; `hypotheses` only shrinks, so it ends.
    ///
    /// A nest is done under its own enclosing scope: an inner port entered on
    /// what an outer one carries is only constant while the outer hypothesis is
    /// open, and an outer port latched from an inner loop's result is only
    /// unrefuted once the inner one is proved. So the inner loops are resolved
    /// inside the outer scope before the outer round reads its own edges, and
    /// again in the base graph once the outer hypothesis is promoted there.
    fn hypothesize(&mut self, loops: Vec<LoopPorts>) {
        let mut order: Vec<usize> = (0..loops.len()).collect();
        order.sort_by_key(|&index| self.nesting(loops[index].op));
        self.hypothesize_within(&loops, &order, None);
    }

    /// The loops `parent` holds directly, resolved in whatever scope is open.
    fn hypothesize_within(&mut self, loops: &[LoopPorts], order: &[usize], parent: Option<OpId>) {
        for &index in order {
            let holder = &loops[index];
            if self.enclosing_loop(loops, holder.op) != parent {
                continue;
            }
            let mut hypotheses: Vec<&Port> = holder
                .ports
                .iter()
                .filter(|port| self.is_constant(port.init))
                .collect();
            while !hypotheses.is_empty() {
                self.eg.push_context();
                for port in &hypotheses {
                    self.eg.union(port.head, port.init);
                }
                self.eg.rebuild();
                self.eg.saturate_rules(
                    &self.ruleset.rewrites,
                    &self.ruleset.interpretation,
                    ITER_LIMIT,
                    NODE_LIMIT,
                );
                self.hypothesize_within(loops, order, Some(holder.op));
                let refuted: Vec<bool> = hypotheses
                    .iter()
                    .map(|port| {
                        port.edges
                            .iter()
                            .any(|&edge| self.eg.find(edge) != self.eg.find(port.init))
                    })
                    .collect();
                self.eg.pop_context();
                if !refuted.contains(&true) {
                    break;
                }
                let mut dropped = refuted.iter();
                hypotheses.retain(|_| !dropped.next().copied().unwrap_or(false));
            }
            let promoted = !hypotheses.is_empty();
            for port in &hypotheses {
                self.eg.union(port.head, port.init);
            }
            self.eg.rebuild();
            // Back to a fixpoint before the next loop opens its scope, from the
            // log the promoted unions left. A loop that promoted nothing left
            // none, and a saturation still costs a round of the rules no delta
            // narrows.
            if promoted {
                self.saturate();
                let mut published = false;
                for port in hypotheses {
                    // Substitution can make the exit constant. A value that
                    // still depends on another changing port stays in the loop.
                    if self.is_constant(port.published) {
                        self.eg.union(port.result, port.published);
                        published = true;
                    }
                }
                if published {
                    self.eg.rebuild();
                    self.saturate();
                }
            }
            self.hypothesize_within(loops, order, Some(holder.op));
        }
    }

    /// The innermost loop of `loops` holding `op`'s own regions.
    fn enclosing_loop(&self, loops: &[LoopPorts], op: OpId) -> Option<OpId> {
        let mut current = self.context.parent_op(op);
        while let Some(holder) = current {
            if loops.iter().any(|other| other.op == holder) {
                return Some(holder);
            }
            current = self.context.parent_op(holder);
        }
        None
    }

    fn is_constant(&self, class: Id) -> bool {
        self.eg.nodes(class).any(|node| node.int().is_some())
    }

    /// How deep in the region tree `op` sits, so the loops are read outermost first.
    fn nesting(&self, op: OpId) -> usize {
        let mut depth = 0;
        let mut current = self.context.parent_op(op);
        while let Some(holder) = current {
            depth += 1;
            current = self.context.parent_op(holder);
        }
        depth
    }

    /// Assume `value == holds` in the current context by unioning its class with the
    /// matching boolean constant. An equality the assumption settles says more than
    /// the truth of the condition: the two operands name one value there, so their
    /// classes are merged as well and every term over either is a term over the
    /// cheapest form of both — the literal, where one side is one.
    fn inject(&mut self, value: ValueId, holds: bool) {
        let cond = self.class_of(value);
        let constant = self
            .eg
            .add(Node::constant(APInt::new(1, holds as u64), Prov::None));
        self.eg.union(cond, constant);
        if let Some((lhs, rhs)) = self.settled_equality(value, holds) {
            self.eg.union(lhs, rhs);
        }
        self.eg.rebuild();
    }

    /// The operand classes a guard proves congruent: an `eq` that holds, or a
    /// `ne` that does not.
    fn settled_equality(&mut self, value: ValueId, holds: bool) -> Option<(Id, Id)> {
        let op = self.context.get_value(value).defining_op()?;
        let instance = self.context.get_op(op);
        if !instance.is::<ops::CmpIOp>() {
            return None;
        }
        let AttributeValue::Predicate(predicate) = instance.attr("predicate")? else {
            return None;
        };
        let equal = match predicate {
            Predicate::Eq => holds,
            Predicate::Ne => !holds,
            _ => return None,
        };
        let [lhs, rhs] = instance.operands()[..] else {
            return None;
        };
        equal.then(|| (self.class_of(lhs), self.class_of(rhs)))
    }

    /// The class standing for `value`, anchoring it as an opaque leaf if the
    /// seeding named none.
    fn class_of(&mut self, value: ValueId) -> Id {
        self.value_class
            .get(&value)
            .copied()
            .unwrap_or_else(|| self.eg.add(Node::input(value)))
    }

    /// The value a node names outright, building nothing.
    fn named_value(&self, node: &Node) -> Option<ValueId> {
        match (node.sym(), node.prov) {
            (_, Prov::Value(value)) => self.context.has_value(value).then_some(value),
            (Some(SymKind::If | SymKind::Theta | SymKind::LoadMemory), Prov::Op(_)) => None,
            (_, Prov::Op(op)) => self
                .context
                .has_operation(op)
                .then(|| self.context.get_op(op).results().first().copied())
                .flatten(),
            _ => None,
        }
    }
}

/// How a literal is written down. A class holds one node per bit pattern however
/// each spelling read it back, so the sign flag on whichever node the class
/// happens to hold says nothing about the value — and a one-bit literal is a
/// truth value, which is `1`, not the `-1` a signed reading of that bit gives.
fn spell(literal: &APInt) -> i64 {
    match literal.width() {
        1 => literal.to_u64() as i64,
        _ => literal.to_i64(),
    }
}
