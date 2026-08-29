use std::collections::HashMap;
use std::fmt::Debug;

use tir_adt::{APFloat, APInt, FxBuildHasher};

use crate::{ClassId, LabelId};

pub(crate) type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

/// An e-node's label: everything about it except its operands. Operands are
/// child class ids carried inline, as the term the label heads. Identity is
/// [`matches`](Label::matches) plus equal canonical children, not `Hash`/`Eq`,
/// so a [`hash_cons`](Label::hash_cons) collision only buckets and never merges
/// nodes.
pub trait Label: Debug + Clone {
    fn children(&self) -> &[ClassId];
    fn children_mut(&mut self) -> &mut [ClassId];

    /// Hash of the complete node, including its children. Congruent nodes must
    /// have equal hashes; collisions are allowed.
    fn hash_cons(&self) -> u64;

    /// Operator-index bucket for pattern search. Contract: `a.matches(b)` implies
    /// `a.op_key() == b.op_key()`, even when `a` is a loosely-matching template —
    /// so the key must use only fields `matches` compares strictly, never
    /// children or a wildcardable field.
    fn op_key(&self) -> u64;

    /// Operator/label equality, ignoring children. Two nodes share a class iff
    /// this holds and their canonical children are equal.
    fn matches(&self, other: &Self) -> bool;

    /// Hash of the label alone. Must agree with [`Self::matches`]: equal labels
    /// hash equal. The default zeroes a copy's children and reuses
    /// [`Self::hash_cons`]; override it when the label's fields can be hashed
    /// without building that copy.
    fn label_hash(&self) -> u64 {
        let mut bare = self.clone();
        for child in bare.children_mut() {
            *child = ClassId(0);
        }
        bare.hash_cons()
    }

    /// Whether `self`, used as a pattern *template*, matches graph node `target`.
    /// Unlike [`matches`](Label::matches) — which is node identity and must stay
    /// strict for hash-consing — a template may treat missing fields as wildcards
    /// (e.g. an untyped template matching any type). The [`op_key`](Label::op_key)
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

    /// This node's constant, spelled the one way the language spells that value
    /// — `None` for a node that is not a ground constant. The *value*, not the
    /// spelling, is what a class is known to be, so a typed and an untyped
    /// spelling of one number must answer with the same term; otherwise a class
    /// proven the number twice would look like a class proven two things.
    ///
    /// Seeds the engine's constant column, so a rule reads a class's constant as
    /// a fact rather than by scanning its rows.
    fn constant(&self) -> Option<Self> {
        None
    }

    /// The type this node carries, as one word — the identity of the language's
    /// type, not its shape. Seeds the engine's type column, so a rule reads the
    /// type of a class it bound as a hole without scanning its rows.
    fn type_key(&self) -> Option<u64> {
        None
    }

    /// A field of the label read as one word: an integer payload, a type, an
    /// attribute. The language numbers its own fields; the engine only moves
    /// the word between an atom's read and a guard's argument.
    fn scalar(&self, _field: u32) -> Option<u64> {
        None
    }

    /// `template` with `fills` written into the named fields — how a head spells
    /// a node whose payload a guard computed. `None` if the language cannot
    /// spell it.
    fn fill(template: &Self, fills: &[(u32, u64)]) -> Option<Self> {
        fills.is_empty().then(|| template.clone())
    }

    /// A unique node gets a fresh class on every insert and never hash-conses or
    /// congruence-merges (effectful ops, distinct unknowns); its operands still
    /// resolve through the union-find.
    fn is_unique(&self) -> bool {
        false
    }

    /// Canonical node for an integer constant, if any; must equal the node the
    /// language interns for that constant.
    fn from_int(_value: APInt) -> Option<Self> {
        None
    }

    /// Canonical node for a float constant, if any.
    fn from_float(_value: APFloat) -> Option<Self> {
        None
    }
}

/// The distinct labels the graph has seen, so a row carries a `u32` where the
/// scalar engine carried a term. Congruence then compares two `u32`s and a slice
/// of child ids instead of calling [`Label::matches`].
///
/// [`Label::label_hash`] sees the operand count, so one label at two arities
/// interns twice. That makes label equality finer than [`Label::matches`], which
/// costs nothing: congruent rows have equal children and so equal arity.
#[derive(Debug)]
pub(crate) struct Labels<L> {
    table: Vec<L>,
    /// [`Label::label_hash`] bucket -> the labels interned under it.
    index: FxHashMap<u64, Vec<LabelId>>,
}

impl<L> Default for Labels<L> {
    fn default() -> Self {
        Self {
            table: Vec::new(),
            index: FxHashMap::default(),
        }
    }
}

impl<L: Label> Labels<L> {
    /// The id of `node`'s label, interning it on first sight. The stored copy
    /// keeps whatever children `node` had; nothing reads them.
    pub(crate) fn intern(&mut self, node: &L) -> LabelId {
        let bucket = self.index.entry(node.label_hash()).or_default();
        for &id in bucket.iter() {
            if self.table[id.index()].matches(node) {
                return id;
            }
        }
        let id = LabelId(self.table.len() as u32);
        bucket.push(id);
        self.table.push(node.clone());
        id
    }

    /// The id of `node`'s label if it has been seen, without interning it.
    pub(crate) fn get(&self, node: &L) -> Option<LabelId> {
        self.index
            .get(&node.label_hash())?
            .iter()
            .copied()
            .find(|&id| self.table[id.index()].matches(node))
    }

    /// The node interned under `id`. Its children are whatever the first node
    /// with this label carried; nothing reads them.
    pub(crate) fn node(&self, id: LabelId) -> &L {
        &self.table[id.index()]
    }

    pub(crate) fn len(&self) -> usize {
        self.table.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Term;

    #[test]
    fn equal_labels_share_an_id_whatever_their_children() {
        let mut labels = Labels::default();
        let a = labels.intern(&Term::op("add", &[ClassId(1), ClassId(2)]));
        let b = labels.intern(&Term::op("add", &[ClassId(7), ClassId(9)]));
        let c = labels.intern(&Term::op("mul", &[ClassId(1), ClassId(2)]));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn get_does_not_intern() {
        let mut labels = Labels::default();
        assert_eq!(labels.get(&Term::leaf("x")), None);
        let id = labels.intern(&Term::leaf("x"));
        assert_eq!(labels.get(&Term::leaf("x")), Some(id));
        assert_eq!(labels.len(), 1);
    }
}
