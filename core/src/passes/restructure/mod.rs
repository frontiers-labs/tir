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

mod branches;
mod cfg;
mod emit;
mod liveness;
mod loops;

use crate::analysis::AnalysisManager;
use crate::func::FuncOp;
use crate::{Context, OperationRef, Pass, PassError, PassTarget, Rewriter};

pub struct RestructurePass;

impl RestructurePass {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RestructurePass {
    fn default() -> Self {
        Self::new()
    }
}

crate::register_pass!(RestructurePass, "restructure");

impl Pass for RestructurePass {
    fn name(&self) -> &'static str {
        "restructure"
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
        if context.get_region(region).block_ids().len() < 2 {
            return Ok(());
        }
        restructure_region(context, region)
    }
}

/// Restructure one CFG region into a single block of structured operations.
fn restructure_region(context: &Context, region: crate::RegionId) -> Result<(), PassError> {
    let mut graph = cfg::Cfg::build(context, region)?;
    loops::restructure(&mut graph);
    let tree = branches::restructure(&mut graph)?;
    let live = liveness::compute(&graph);
    emit::emit(context, region, &graph, &tree, &live)
}
