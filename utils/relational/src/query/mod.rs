//! Conjunctive queries over a [`Database`](crate::Database): atoms over its
//! tables, lattice columns, id spaces and client tables, guards over the words a match
//! binds, and an evaluator that answers with loops over the tables rather than
//! a walk over a goal stack.
//!
//! A [`Query`] is the IR; a [`Plan`] is that query with an evaluation order
//! chosen once. The order a plan produces matches in is part of the contract:
//! a client applies heads in match order, so the ids a saturation mints depend
//! on it.
//!
//! A row atom can hold its tag to an interval ([`Rows::tag_within`]): reached
//! through its value, it reads that interval's rows alone from the table's
//! [`TagOrder`](crate::store::TagOrder); reached any other way, guards the
//! planner adds filter its rows.
//!
//! Where two consecutive row atoms read a variable neither has bound yet nor
//! reaches its rows through, the evaluator joins them over tries of their
//! tables, intersecting the cells each atom allows for that variable, and
//! then emits the matches in the order the loop nest would have found them. The run is first split
//! into a tree of bags, a generalized hypertree decomposition: atoms that
//! close a cycle share a bag and are joined that way, each child bag is held
//! to the cells its parent gives the variables they share, and the bags'
//! matches are combined through those cells.

mod eval;
mod ghd;
mod ir;
mod join;
mod plan;
mod set;
pub(crate) mod trie;

pub(crate) use eval::Unchecked;
pub use eval::{Allow, Matches, PlanCache, Prepared, UNBOUND, bound};
pub use ir::{
    Atom, Calls, Cmp, Expr, ExternId, Guard, Host, Lattice, Nested, Out, Param, Place, Query, Read,
    Resolution, Rows, Scalar, Select, Swap, Term, Tuples, Var,
};
pub use plan::{Plan, Step};
