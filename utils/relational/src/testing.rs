//! A tiny term language the crate's own tests are written in.

use tir_adt::FxHasher;

use crate::{Carrier, ClassId, Label};
use std::hash::{Hash, Hasher};

/// `op(children…)`, plus the properties the engine branches on.
///
/// A term with a non-zero `width` has an integer carrier of that many bits,
/// keyed by the width, and there `add`, `sub`, `neg` and `not` carry their
/// offset laws and `#` with a `value` is an integer constant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub op: String,
    pub children: Vec<ClassId>,
    pub commutative: bool,
    pub width: u8,
    pub value: Option<u64>,
    /// The type the term carries, which is not part of its identity.
    pub ty: Option<u64>,
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
            width: 0,
            value: None,
            ty: None,
        }
    }

    /// `op` declared commutative in its two operands.
    pub fn comm(op: &str, children: &[ClassId]) -> Self {
        Self {
            commutative: true,
            ..Self::op(op, children)
        }
    }

    pub fn int(value: i64) -> Self {
        Self::leaf(&value.to_string())
    }

    /// `op` at an integer carrier `width` bits wide.
    pub fn typed(op: &str, width: u8, children: &[ClassId]) -> Self {
        Self {
            width,
            commutative: op == "add",
            ..Self::op(op, children)
        }
    }

    /// The integer constant `value` of the carrier `width` bits wide.
    pub fn num(width: u8, value: u64) -> Self {
        Self {
            value: Some(value),
            ..Self::typed("#", width, &[])
        }
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
        self.width.hash(&mut h);
        self.value.hash(&mut h);
        self.children.hash(&mut h);
        h.finish()
    }

    fn op_key(&self) -> u64 {
        let mut h = FxHasher::default();
        self.op.hash(&mut h);
        h.finish()
    }

    fn matches(&self, other: &Self) -> bool {
        self.op == other.op && self.width == other.width && self.value == other.value
    }

    /// A leaf spelled as a number is a literal.
    fn constant(&self) -> Option<Self> {
        (self.children.is_empty() && self.op.parse::<i64>().is_ok()).then(|| self.clone())
    }

    fn type_key(&self) -> Option<u64> {
        self.ty
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
            [(0, value)] if template.width != 0 => Some(Term::num(template.width, *value)),
            [(0, value)] => Some(Term::int(*value as i64)),
            _ => None,
        }
    }

    fn commutative(&self) -> bool {
        self.commutative
    }

    fn carrier(&self) -> Option<Carrier> {
        (self.width != 0).then_some(Carrier {
            width: self.width,
            key: u64::from(self.width),
        })
    }

    fn offset_coefficient(&self, operand: usize) -> Option<i64> {
        match (self.op.as_str(), operand) {
            ("add", _) | ("sub", 0) => Some(1),
            ("sub", 1) | ("neg", 0) | ("not", 0) => Some(-1),
            _ => None,
        }
    }

    fn bias(&self) -> u64 {
        match self.op.as_str() {
            "not" => u64::MAX,
            _ => 0,
        }
    }

    fn identity(&self, operand: usize) -> Option<u64> {
        match (self.op.as_str(), operand) {
            ("add", _) | ("sub", 1) => Some(0),
            _ => None,
        }
    }

    fn int_value(&self) -> Option<u64> {
        self.value
    }

    fn constant_of(&self, value: u64) -> Option<Self> {
        (self.width != 0).then(|| Term::num(self.width, value))
    }
}
