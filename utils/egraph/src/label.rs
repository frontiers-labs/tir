use std::fmt::Debug;

use crate::{ClassId, FxHashMap, LabelId};

/// An e-node's label: everything about it except its operands. Operands are
/// child class ids carried inline, as the term the label heads; an operand's
/// offset, when it has one, travels beside the node (see
/// [`crate::Engine::insert`]). Identity is
/// [`matches`](Label::matches) plus equal canonical children, not `Hash`/`Eq`,
/// so a [`hash_cons`](Label::hash_cons) collision only buckets and never merges
/// nodes.
pub trait Label: Debug + Clone + Send + Sync {
    fn children(&self) -> &[ClassId];
    fn children_mut(&mut self) -> &mut [ClassId];

    /// Hash of the complete node, including its children. Congruent nodes must
    /// have equal hashes; collisions are allowed.
    fn hash_cons(&self) -> u64;

    /// What [`Self::sort_key`] reads the label's interned fields through:
    /// the tables that spell a type or a name the label holds by id. `()` for
    /// a label that holds none.
    type Names: Clone + Send + Sync;

    /// What labels are ordered by; see [`Self::sort_key`].
    type SortKey: Ord + Clone + Send + Sync;

    /// The label's content as a key: its operator, arity, payload and type,
    /// each as what it is and never as a hash or as an id handed out in the
    /// order things were met. A search reads labels in the order of their
    /// keys, so what it finds first does not depend on which graphs the
    /// vocabulary served before. Labels that differ have different keys; two
    /// that share one keep the order they were met in.
    fn sort_key(&self, names: &Self::Names) -> Self::SortKey;

    /// Operator-index bucket for pattern search. Contract: `a.matches(b)` implies
    /// `a.op_key() == b.op_key()`; never children.
    fn op_key(&self) -> u64;

    /// The [`Self::op_key`] bucket a search for `self` as a template reads, or
    /// `None` when it matches labels of more than one bucket and the search
    /// reads every label. Contract: `a.matches_template(b)` implies
    /// `a.template_key()` is `None` or `Some(b.op_key())`, so the key uses
    /// only fields the template compares strictly.
    fn template_key(&self) -> Option<u64> {
        Some(self.op_key())
    }

    /// Operator/label equality, ignoring children. Two nodes share a class iff
    /// this holds and their canonical children are equal.
    fn matches(&self, other: &Self) -> bool;

    /// This node as the label table keeps it: everything [`Self::matches`]
    /// compares and nothing that names the graph the node came from, its
    /// provenance say. The table outlives that graph.
    fn bare(&self) -> Self {
        self.clone()
    }

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
    /// (e.g. an untyped template matching any type). Its search reads the
    /// bucket [`template_key`](Label::template_key) names.
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
    /// a fact rather than by scanning its rows. Not asked of a carrier's
    /// integer constant, which is a reference rather than a row.
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
    /// attribute. The language numbers its own fields, and may spell a word
    /// through `names`; the engine only moves the word between an atom's
    /// read and a guard's argument.
    fn scalar(&self, _names: &Self::Names, _field: u32) -> Option<u64> {
        None
    }

    /// `template` with `fills` written into the named fields — how a head spells
    /// a node whose payload a guard computed. `None` if the language cannot
    /// spell it.
    fn fill(template: &Self, _names: &Self::Names, fills: &[(u32, u64)]) -> Option<Self> {
        fills.is_empty().then(|| template.clone())
    }

    /// A unique node gets a fresh class on every insert and never hash-conses or
    /// congruence-merges (effectful ops, distinct unknowns); its operands still
    /// resolve through the union-find.
    fn is_unique(&self) -> bool {
        false
    }

    // ---- offset laws --------------------------------------------------------
    //
    // Intrinsic properties of the operator, read once per label and cached:
    // definitions the engine builds identity from, not rewrites.

    /// The carrier of this node's result when it is an integer or a pointer
    /// of at most 64 bits: what references to its class add offsets in.
    /// `None` keeps every reference to the class at offset zero, and disables
    /// the node's coefficients and identities.
    fn carrier(&self) -> Option<Carrier> {
        None
    }

    /// How a constant added to operand `operand` moves to the result:
    /// `op(.., x + c, ..) == op(.., x, ..) + coefficient * c`, with `c`
    /// reduced into the result's width. Add is `1, 1`; sub `1, -1`; negate
    /// `-1`; pointer plus integer `1, 1`. `None` keeps the operand's offset as
    /// part of the node's identity, so `f(x + 1)` and `f(x)` stay apart.
    ///
    /// A node every operand of which has a coefficient is linear: with every
    /// operand a constant it is [`Self::bias`] plus the constant the
    /// coefficients sum them to.
    fn offset_coefficient(&self, _operand: usize) -> Option<i64> {
        None
    }

    /// A linear node's value with every operand zero: zero for add, sub and
    /// negate, all ones for bitwise not (`not(c) == -1 - c`).
    fn bias(&self) -> u64 {
        0
    }

    /// For a binary node, the constant at `operand` that makes the node equal
    /// its other operand: zero on either side of an add, on the right of a
    /// sub. The other operand's coefficient, if any, must be one.
    fn identity(&self, _operand: usize) -> Option<u64> {
        None
    }

    /// This node's value when it is an integer constant of its
    /// [`Self::carrier`], in any representative modulo 2^width. Such a node is
    /// never stored: it is the reference `(zero, value)`, `zero` being the
    /// carrier's zero class, minted from [`Self::constant_of`]`(0)`. Asked of
    /// every inserted leaf, so it must be cheap.
    fn int_value(&self) -> Option<u64> {
        None
    }

    /// The integer constant `value` of this node's carrier: the zero the
    /// carrier's constants are offsets of, and how a constant reference is
    /// spelled back as a node.
    fn constant_of(&self, _value: u64) -> Option<Self> {
        None
    }
}

/// The type a value's offsets live in: an integer or a pointer of at most 64
/// bits. Each key has one zero class, which every constant of the carrier is
/// an offset of, so two carriers of one width stay apart (a 64-bit integer and
/// a pointer) and two spellings of one type share their constants by sharing
/// a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Carrier {
    pub width: u8,
    pub key: u64,
}

/// What the engine reads off a label's [`Label`] laws, once per label.
#[derive(Clone, Debug, Default)]
pub(crate) struct Laws {
    /// The [`tir_relational::unionfind::mask`] of the result's carrier, zero for none.
    pub(crate) mask: u64,
    pub(crate) carrier: Option<Carrier>,
    /// Per operand, its offset coefficient; zero for none. Empty when no
    /// operand has one.
    pub(crate) coefficients: smallvec::SmallVec<[i64; 2]>,
    /// For a binary node, the identity element each operand has, if any.
    pub(crate) identity: [Option<u64>; 2],
    /// For a leaf integer constant of the carrier, its value.
    pub(crate) constant: Option<u64>,
    /// A linear node's value over zero operands.
    pub(crate) bias: u64,
}

impl Laws {
    pub(crate) fn of<L: Label>(node: &L) -> Self {
        let Some(carrier) = node.carrier() else {
            return Self::default();
        };
        let mask = tir_relational::unionfind::mask(carrier.width);
        let arity = node.children().len();
        if node.is_unique() {
            return Self {
                mask,
                carrier: Some(carrier),
                ..Self::default()
            };
        }
        let mut coefficients: smallvec::SmallVec<[i64; 2]> = (0..arity)
            .map(|operand| node.offset_coefficient(operand).unwrap_or(0))
            .collect();
        if coefficients.iter().all(|&coefficient| coefficient == 0) {
            coefficients.clear();
        }
        let identity = match arity {
            2 => [0, 1].map(|operand| node.identity(operand).map(|value| value & mask)),
            _ => [None; 2],
        };
        let constant = (arity == 0)
            .then(|| node.int_value())
            .flatten()
            .map(|value| value & mask);
        Self {
            mask,
            carrier: Some(carrier),
            coefficients,
            identity,
            constant,
            bias: node.bias() & mask,
        }
    }

    pub(crate) fn coefficient(&self, operand: usize) -> i64 {
        self.coefficients.get(operand).copied().unwrap_or(0)
    }

    pub(crate) fn linear(&self) -> bool {
        !self.coefficients.is_empty()
            && self
                .coefficients
                .iter()
                .all(|&coefficient| coefficient != 0)
    }
}

/// The distinct labels the graph has seen, so a row carries a `u32` where the
/// scalar engine carried a term. Congruence then compares two `u32`s and a slice
/// of child ids instead of calling [`Label::matches`].
///
/// One label at two arities interns twice: a label names a table, and a table
/// has one arity. That makes label equality finer than [`Label::matches`],
/// which costs nothing: congruent rows have equal children and so equal arity.
#[derive(Clone, Debug)]
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
    pub(crate) fn intern(&mut self, node: &L) -> LabelId {
        let bucket = self.index.entry(node.label_hash()).or_default();
        for &id in bucket.iter() {
            let known = &self.table[id.index()];
            if known.children().len() == node.children().len() && known.matches(node) {
                return id;
            }
        }
        let id = LabelId(self.table.len() as u32);
        bucket.push(id);
        self.table.push(node.bare());
        id
    }

    pub(crate) fn get(&self, node: &L) -> Option<LabelId> {
        self.index
            .get(&node.label_hash())?
            .iter()
            .copied()
            .find(|&id| {
                let known = &self.table[id.index()];
                known.children().len() == node.children().len() && known.matches(node)
            })
    }

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
