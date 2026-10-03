//! The relational engine behind TIR's e-graph, in two layers.
//!
//! [`store`] is the generic one: tables of `u32` cells whose key columns
//! determine a value column, with the whole-column loops done by
//! [`tir_adt::simd`]. It holds any graph and knows nothing about classes.
//!
//! [`Engine`] is the e-graph on top of it. Each label is a table from child
//! classes to the class the node belongs to, so hash-consing is a key lookup,
//! congruence is the table's functional dependency, and a rewrite's left-hand
//! side is a join. Rewrites are conjunctive queries; saturation is the least
//! fixpoint of the rule set.

mod column;
mod csr;
mod engine;
mod extract;
mod label;
mod query;
mod rule;
mod saturate;
pub mod store;
mod telemetry;
mod unionfind;

#[cfg(test)]
mod testing;

pub use column::Fact;
pub use csr::Csr;
pub use engine::{ClassRef, Engine, Rows, Stats};
pub use extract::Extraction;
pub use label::Label;
pub use query::{
    Atom, Cmp, ColumnId, Expr, Externs, Field, Guard, Match, Nested, NoExterns, Plan, Query,
    Scalar, Source, Step, Var,
};
pub use rule::{HeadOp, LabelFill, Rule};
pub use saturate::{Delta, round_roots};
pub use telemetry::{RoundStats, Timer, report_saturation};

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
