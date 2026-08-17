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
use crate::builtin::FuncOp;
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
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::FuncOp;
    use crate::parse::ir::parse_ir;
    use crate::{Operation, PassManager};

    /// A ring of blocks, each branching to the next two: strongly connected,
    /// entered at two of its members, and irreducible however it is traversed.
    fn tangle(blocks: usize) -> String {
        let mut source = String::from("func @tangle(%0: !i1, %1: !i32) -> !i32 {\n");
        source.push_str("  cfg.cond_br %0, ^bb1, ^bb2\n");
        for block in 1..=blocks {
            let next = block % blocks + 1;
            let after = (block + 1) % blocks + 1;
            source.push_str(&format!("^bb{block}:\n"));
            source.push_str(&format!("  %v{block} = addi %1, %1 : !i32\n"));
            if block == blocks {
                source.push_str(&format!("  cfg.cond_br %0, ^bb{next}, ^bb{}\n", blocks + 1));
            } else {
                source.push_str(&format!("  cfg.cond_br %0, ^bb{next}, ^bb{after}\n"));
            }
        }
        source.push_str(&format!("^bb{}:\n  return %1\n}}\n", blocks + 1));
        source
    }

    fn count_ops(context: &Context, op: crate::OpId) -> usize {
        let instance = context.get_op(op);
        let mut total = 1;
        for region in instance.regions() {
            for block in context.get_region(region).block_ids() {
                for nested in context.get_block(block).op_ids() {
                    total += count_ops(context, nested);
                }
            }
        }
        total
    }

    fn restructure_source(source: &str) -> (Context, FuncOp, usize) {
        let context = Context::with_default_dialects();
        let func = parse_ir::<FuncOp>(&context, source).expect("parse");
        let before = count_ops(&context, func.id());
        let mut manager = PassManager::new();
        manager.add_pass(RestructurePass::new());
        manager
            .run(&context, context.get_op(func.id()))
            .expect("restructure");
        (context, func, before)
    }

    /// Restructuring copies no node, so the output grows linearly: over a
    /// pathological irreducible graph every added block costs the same fixed
    /// number of operations (the dispatch constants, one conditional and its
    /// yields), and the output stays within 6x the input.
    #[test]
    fn a_pathological_graph_grows_linearly() {
        let mut measured = Vec::new();
        for blocks in [4, 8, 16, 32] {
            let (context, func, before) = restructure_source(&tangle(blocks));
            assert!(
                func.verify(&context).is_ok(),
                "restructured IR must verify: {:?}",
                func.verify(&context)
            );
            let after = count_ops(&context, func.id());
            assert!(
                after <= 6 * before,
                "{blocks} blocks: {before} operations became {after}"
            );
            measured.push((blocks, after));
        }

        let per_block = measured
            .windows(2)
            .map(|step| (step[1].1 - step[0].1) / (step[1].0 - step[0].0))
            .collect::<Vec<_>>();
        assert!(
            per_block.windows(2).all(|step| step[0] == step[1]),
            "every added block must cost the same: {per_block:?} from {measured:?}"
        );
    }
}
