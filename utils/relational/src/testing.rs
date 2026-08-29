//! A tiny term language the crate's own tests are written in.

use tir_adt::{APInt, FxHasher};

use crate::{ClassId, Label};
use std::hash::{Hash, Hasher};

/// `op(children…)`, plus the two properties the engine branches on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub op: String,
    pub children: Vec<ClassId>,
    pub commutative: bool,
    pub unique: bool,
}

impl Term {
    pub fn leaf(op: &str) -> Self {
        Self::op(op, &[])
    }

    pub fn op(op: &str, children: &[ClassId]) -> Self {
        Self {
            op: op.to_string(),
            children: children.to_vec(),
            commutative: false,
            unique: false,
        }
    }

    /// `op` declared commutative in its two operands.
    pub fn comm(op: &str, children: &[ClassId]) -> Self {
        Self {
            commutative: true,
            ..Self::op(op, children)
        }
    }

    /// A node that never hash-conses and never congruence-merges.
    pub fn unique(op: &str, children: &[ClassId]) -> Self {
        Self {
            unique: true,
            ..Self::op(op, children)
        }
    }

    pub fn int(value: i64) -> Self {
        Self::leaf(&value.to_string())
    }
}

impl Label for Term {
    fn children(&self) -> &[ClassId] {
        &self.children
    }

    fn children_mut(&mut self) -> &mut [ClassId] {
        &mut self.children
    }

    fn hash_cons(&self) -> u64 {
        let mut h = FxHasher::default();
        self.op.hash(&mut h);
        self.children.hash(&mut h);
        h.finish()
    }

    fn op_key(&self) -> u64 {
        let mut h = FxHasher::default();
        self.op.hash(&mut h);
        h.finish()
    }

    fn matches(&self, other: &Self) -> bool {
        self.op == other.op
    }

    /// A leaf spelled as a number is a literal.
    fn constant(&self) -> Option<Self> {
        (self.children.is_empty() && self.op.parse::<i64>().is_ok()).then(|| self.clone())
    }

    /// Field 0 is the literal's value.
    fn scalar(&self, field: u32) -> Option<u64> {
        (field == 0)
            .then(|| self.op.parse::<i64>().ok())
            .flatten()
            .map(|v| v as u64)
    }

    fn fill(template: &Self, fills: &[(u32, u64)]) -> Option<Self> {
        match fills {
            [] => Some(template.clone()),
            [(0, value)] => Some(Term::int(*value as i64)),
            _ => None,
        }
    }

    fn commutative(&self) -> bool {
        self.commutative
    }

    fn is_unique(&self) -> bool {
        self.unique
    }

    fn from_int(value: APInt) -> Option<Self> {
        Some(Term::int(value.to_u64() as i64))
    }
}
