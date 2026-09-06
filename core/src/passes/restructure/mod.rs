//! Total control-flow restructuring: any CFG region becomes structured `scf`.
//!
//! Follows Bahmann and Reissmann, "Perfect Reconstructability of Control Flow
//! from Demand Dependence Graphs": loops are restructured first, one
//! single-entry single-exit tail-controlled loop per strongly connected
//! component, with dispatch predicates naming the entry and the exit an
//! iteration took; the acyclic graph that leaves behind is then restructured
//! into a tree of conditionals, with a continuation predicate wherever a branch
//! has several join points. Neither phase copies a node, so the output grows
//! linearly with the input.
//!
//! The pass reads the CFG through [`Terminator`], [`BranchTerminator`] and
//! [`BranchGuard`] only, so it restructures any dialect's control flow. The
//! operations it *creates* are `scf` ones plus the integer constants the
//! predicates need.
//!
//! `restructure-nodes` produces unordered regions of `scf.switch`, `scf.loop`
//! and `scf.for`, constructing memory order first, since an unordered region
//! keeps none of its own.

mod branches;
mod cfg;
mod deps;
mod emit_nodes;
mod liveness;
mod loops;
mod ports;

use crate::analysis::AnalysisManager;
use crate::func::FuncOp;
use crate::{Context, OperationRef, Pass, PassError, PassTarget, Rewriter};

/// The unordered-region conversion. A function whose body is already
/// unordered is left alone; any ordered one is converted, a single block
/// included. Memory order is constructed where the function touches memory
/// and carries no dependencies yet.
pub struct RestructureNodesPass;

impl RestructureNodesPass {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RestructureNodesPass {
    fn default() -> Self {
        Self::new()
    }
}

crate::register_pass!(RestructureNodesPass, "restructure-nodes");

impl Pass for RestructureNodesPass {
    fn name(&self) -> &'static str {
        "restructure-nodes"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let region = op.op().regions()[0];
        if context.get_region(region).is_nodes() {
            return Ok(());
        }
        restructure_region(context, region, deps::wants_chain(context, region))
    }
}

/// Restructure one CFG region into an unordered region of structured
/// operations, constructing memory order on the way when `thread`.
fn restructure_region(
    context: &Context,
    region: crate::RegionId,
    thread: bool,
) -> Result<(), PassError> {
    let mut graph = cfg::Cfg::build(context, region, thread)?;
    loops::restructure(&mut graph);
    let tree = branches::restructure(&mut graph)?;
    let live = liveness::compute(&graph);
    emit_nodes::emit(context, region, &graph, &tree, &live)
}
