//! The relational engine behind TIR's e-graph: e-nodes as rows of columnar
//! relations, congruence as a bulk group-by, e-matching as a join.
//!
//! An e-graph is a database. Each operator family is a relation whose columns
//! are the child classes plus the class the row belongs to, and congruence is
//! the functional dependency `(label, children) -> class`. Rewrites are
//! conjunctive queries; saturation is the least fixpoint of the rule set. The
//! layout follows: every hot loop is a loop over `Vec<u32>` columns, not a walk
//! over per-node allocations.

mod csr;
mod engine;
mod label;
mod unionfind;

#[cfg(test)]
mod testing;

pub use csr::Csr;
pub use engine::{ClassRef, Engine, Rows, Stats};
pub use label::{Label, Labels};
pub use unionfind::UnionFind;

/// An e-class. Only [`UnionFind::find`] turns one into its canonical form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClassId(pub u32);

impl ClassId {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    pub fn from_raw(raw: u32) -> Self {
        ClassId(raw)
    }
}

/// A row of one relation: an e-node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowId(pub u32);

impl RowId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// An interned label: an e-node stripped of its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LabelId(pub u32);

impl LabelId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// One relation of the database: an operator family at a fixed arity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelId(pub u32);

impl RelId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}
