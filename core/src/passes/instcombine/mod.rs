//! InstCombine: an equality-saturation simplifier on the shared
//! `crate::rewrite` driver. It seeds the function's regions
//! (`seed`, which reads gates off the ops' own interfaces)
//! into a [`tir_egraph`] e-graph of real IR values, saturates, extracts the
//! cheapest form per value by [`crate::OpCost`], and rewrites what improved.
//!
//! Flow-sensitive facts ride the e-graph's scoped assumptions, both ways round. A
//! structured region pushes its guard's condition around its body and pops it on
//! the way back out; a loop's carried port is *hypothesised* to hold the constant
//! the loop was entered on, and what no edge back into it refutes is promoted
//! into the base graph. The region tree is the dominance, so nothing here
//! computes one.
//!
//! What a rewrite leaves behind is the same commit's business: the operation it
//! took the readers of, and every operation only that one read, are erased on the
//! way out.
//!
//! The engine holds no op-specific knowledge — identity, cost, folding and
//! constant-reading come from op interfaces; op construction is owned by the rewrites.

mod nodes;
pub(crate) mod rules;
mod state;

pub use nodes::InstCombineNodesPass;
pub(crate) use nodes::sweep_demanded;
