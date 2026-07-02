use std::fmt::Debug;

use tir_adt::{APFloat, APInt};

use super::Id;

/// An e-graph node. Operands are child e-class [`Id`]s carried inline. Identity is
/// [`matches`](ENode::matches) plus equal canonical children, not `Hash`/`Eq`, so a
/// [`hash_cons`](ENode::hash_cons) collision only buckets and never merges nodes.
pub trait ENode: Debug + Clone {
    fn children(&self) -> &[Id];
    fn children_mut(&mut self) -> &mut [Id];

    /// Bucket hash for hash-consing. Not required collision-free.
    fn hash_cons(&self) -> u64;

    /// Operator-index bucket for pattern search ([`EGraph::classes_with_op`]).
    /// Contract: `a.matches(b)` implies `a.op_key() == b.op_key()`, even when `a` is a
    /// loosely-matching template — so the key must use only fields `matches` compares
    /// strictly, never a wildcardable one. Default [`hash_cons`](ENode::hash_cons);
    /// override when a template matches beyond its own bucket.
    fn op_key(&self) -> u64 {
        self.hash_cons()
    }

    /// Operator/label equality, ignoring children. Two nodes share a class iff this
    /// holds and their canonical children are equal.
    fn matches(&self, other: &Self) -> bool;

    /// Whether `self`, used as a pattern *template*, matches graph node `target`.
    /// Unlike [`matches`](ENode::matches) — which is node identity and must stay
    /// strict for hash-consing — a template may treat missing fields as wildcards
    /// (e.g. an untyped template matching any type). The [`op_key`](ENode::op_key)
    /// contract extends to this relation: `a.matches_template(b)` implies
    /// `a.op_key() == b.op_key()`.
    fn matches_template(&self, target: &Self) -> bool {
        self.matches(target)
    }

    /// Whether the operator is commutative in its two operands; pattern search
    /// then tries both operand orders.
    fn commutative(&self) -> bool {
        false
    }

    /// A unique node gets a fresh class on every `add` and never hash-conses or
    /// congruence-merges (effectful ops, distinct unknowns); its operands still
    /// resolve through `find`.
    fn is_unique(&self) -> bool {
        false
    }

    /// Canonical node for an integer constant, if any. Backs [`Var::Int`](super::Var::Int)
    /// leaves; must equal the node the language interns for that constant.
    fn from_int(_value: APInt) -> Option<Self> {
        None
    }

    /// Canonical node for a float constant, if any. Backs [`Var::Float`](super::Var::Float)
    /// leaves; see [`from_int`](ENode::from_int).
    fn from_float(_value: APFloat) -> Option<Self> {
        None
    }
}
