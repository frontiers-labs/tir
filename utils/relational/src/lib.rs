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
mod query;
mod unionfind;

#[cfg(test)]
mod testing;

pub use csr::Csr;
pub use engine::{ClassRef, Engine, Rows, Stats};
pub use label::Label;
pub use query::{Atom, Match, Plan, Query, Var};
pub use unionfind::UnionFind;

/// Whether `TIR_SAT_TRACE` asked for a saturation trace on stderr: every class
/// minted (`A id node children`) and every merge (`U a b -> survivor`), plus
/// whatever the drivers add.
///
/// The trace is how an engine change is localized. Two compilers built from
/// different commits assign the same ids for as long as they agree, so the first
/// differing line of the diff is the coordinates of the divergence — the round,
/// the rule and the match that caused it — rather than an object file that
/// merely came out different.
pub fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TIR_SAT_TRACE").is_some_and(|value| value != "0"))
}

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
