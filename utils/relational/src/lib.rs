//! A relational engine: tables of ids, the equalities over them, and the
//! queries and fixpoints a compiler runs over both.
//!
//! [`store`] holds tables of `u32` cells, each optionally weighted, whose key
//! columns determine a value column, with the whole-column loops done by
//! [`tir_adt::simd`]. A table holds any relation and knows nothing about what
//! its cells name.
//!
//! [`Database`] puts tables together with what their cells name: id spaces,
//! each partitioned by a union-find that places every id at a constant offset
//! from its representative, so a cell and its weight read as a [`Ref`]. It
//! pushes the equalities into the tables on [`Database::canonicalize`],
//! carrying the offsets along and reporting the rows that came to share a key.
//! It also keeps lattice columns of what an id is known to be, the epoch
//! stamps and change log a semi-naive fixpoint reads, the indexes (rows grouped
//! by a column, ids to the rows naming them) built once per change, and
//! assumption scopes that snapshot all of it along with the client's own state.
//!
//! [`chunk`] is the other physical layout: relations keyed by a row id, as
//! positional columns and clustered tables cut into chunks a partition owns,
//! with [`grouping`] for the access paths by any other cell.
//!
//! [`query`] is conjunctive queries over a database, with an evaluator that
//! answers by loops over the tables, or by a worst-case optimal join over
//! sorted tries where several atoms share a variable; [`rule`] is what a match
//! writes;
//! [`saturate`] is the semi-naive fixpoint over a rule set; [`extract`] is the
//! least-cost fold a client reads a solution off with. They name tables,
//! columns and cells, and ask their client for whatever means more than that.
//!
//! [`Words`] is a client's own table: rows of words, built once and shared by
//! every database whose queries range over it, for what a rule set knows and
//! no id names.
//!
//! The e-graph is one such client and lives in its own crate, `tir-egraph`.

pub mod chunk;
mod column;
mod csr;
mod db;
pub mod extract;
pub mod grouping;
pub mod query;
pub mod rule;
pub mod saturate;
pub mod store;
mod telemetry;
pub mod unionfind;
mod words;

pub use column::{Column, Fact, Join};
pub use csr::Csr;
pub use db::{
    Core, Database, Dissolve, Groups, LatticeId, Marks, Placed, Placement, SpaceId, Stats,
};
pub use query::{Cmp, Expr, Scalar, Step, Var};
pub use telemetry::{RoundStats, Timer, report_saturation};
pub use words::Words;

pub(crate) type FxHashMap<K, V> = std::collections::HashMap<K, V, tir_adt::FxBuildHasher>;

/// Whether `TIR_SAT_TRACE` asked for a saturation trace on stderr: every id
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

/// A reference to an id of a space: the id plus a constant, read as "the id's
/// value plus `offset`" modulo 2^W, W being the width of the id's carrier. An
/// id without a carrier is only ever referred to at offset zero. Only
/// [`Core::find`] turns one into its canonical form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ref {
    pub id: u32,
    pub offset: u64,
}

impl Ref {
    pub fn new(id: u32, offset: u64) -> Self {
        Self { id, offset }
    }
}

impl From<u32> for Ref {
    fn from(id: u32) -> Self {
        Self { id, offset: 0 }
    }
}
