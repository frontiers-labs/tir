//! Dead code elimination shared by SSA functions and machine symbols: a
//! worklist over [`DefUse`] chains erases pure ops whose every virtual def
//! (SSA result or Def-role register attribute) is unused, retiring the erased
//! op's reads so newly dead producers are revisited without rescanning.
//!
//! A block no execution reaches is dead the same way: `sccp` leaves its
//! executability in [`ConstantFacts`], and the blocks it never reached go with
//! everything they held.
//!
//! In backend pipelines it must run before register allocation — a
//! physical-register write counts as a side effect, so nothing is eligible
//! after allocation.

use std::collections::{HashMap, HashSet};

use crate::analysis::{ConstantFacts, DefUse, RegRef, op_regs};
use crate::backend::SymbolOp;
use crate::{
    AnalysisManager, BlockId, Context, MemoryWrite, OpHandle, OpId, OperationRef, Pass, PassError,
    PassTarget, Rewriter, Terminator, func::FuncOp,
};

#[derive(Default)]
pub struct DeadCodeEliminationPass;

impl DeadCodeEliminationPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(DeadCodeEliminationPass, "dce");

impl Pass for DeadCodeEliminationPass {
    fn name(&self) -> &'static str {
        "dce"
    }

    // Anchors on both SSA functions and machine symbols; a target can name only
    // one op, so the match happens in `run`.
    fn target(&self) -> PassTarget {
        PassTarget::Any
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() && op.as_op::<SymbolOp>().is_none() {
            return Ok(());
        }

        let defuse = analyses.get::<DefUse>(context, op.op().id);
        // Live read counts, retired as dead readers are erased.
        let mut use_counts = defuse.use_counts();
        // LIFO over walk order visits consumers before their producers.
        let mut queue: Vec<_> = defuse.ops().to_vec();

        while let Some(op_id) = queue.pop() {
            if !context.has_operation(op_id) {
                continue;
            }
            let instance = context.get_op(op_id);
            if !is_erasable(&instance, &use_counts) {
                continue;
            }

            let block = instance.parent_block().map(|b| context.get_block(b));
            // Read before the erase: the op's storage goes away with it.
            let used_regs = op_regs(&instance).uses;
            rewriter.erase_op(&OperationRef::new(instance.clone(), block, None))?;

            for used in used_regs {
                let RegRef::Virtual { id, .. } = used else {
                    continue;
                };
                if let Some(count) = use_counts.get_mut(&id) {
                    *count -= 1;
                    if *count == 0 {
                        queue.extend_from_slice(defuse.defs_of(id));
                    }
                }
            }
        }

        if op.as_op::<FuncOp>().is_some() {
            erase_unreached_blocks(context, rewriter, analyses, op.op().id)?;
        }
        Ok(())
    }
}

/// Erase every block no execution reaches, along with the operations it held. A
/// block a surviving branch still names stays put: rewriting that branch is not
/// this pass's business.
fn erase_unreached_blocks(
    context: &Context,
    rewriter: &mut Rewriter,
    analyses: &AnalysisManager,
    root: OpId,
) -> Result<(), PassError> {
    let facts = analyses.get::<ConstantFacts>(context, root);
    for region in super::regions_under(context, root) {
        if !context.has_region(region) {
            continue;
        }
        let blocks = context.get_region(region).block_ids();
        // A region control never entered says nothing about what is dead inside
        // it: the solver only reasons about blocks it reached the entry of.
        match blocks.first() {
            Some(&entry) if facts.is_executable(entry) => {}
            _ => continue,
        }
        // What survives: the blocks control reaches, and — since no branch is
        // rewritten here — whatever a surviving branch still names, transitively.
        let mut kept: HashSet<BlockId> = blocks
            .iter()
            .copied()
            .filter(|&block| facts.is_executable(block))
            .collect();
        let mut queue: Vec<BlockId> = blocks
            .iter()
            .copied()
            .filter(|block| kept.contains(block))
            .collect();
        while let Some(block) = queue.pop() {
            for successor in successors(context, block) {
                if kept.insert(successor) {
                    queue.push(successor);
                }
            }
        }
        for &block in &blocks {
            if kept.contains(&block) {
                continue;
            }
            let handle = context.get_block(block);
            for op_id in handle.op_ids().into_iter().rev() {
                let target = OperationRef::new(context.get_op(op_id), Some(handle.clone()), None);
                rewriter.erase_op(&target)?;
            }
            rewriter.erase_block(block);
        }
    }
    Ok(())
}

/// The blocks `block`'s terminator branches to.
fn successors(context: &Context, block: BlockId) -> Vec<BlockId> {
    context
        .get_block(block)
        .op_ids()
        .last()
        .map(|&terminator| context.get_op(terminator))
        .and_then(|terminator| terminator.as_interface::<dyn Terminator>())
        .map(|terminator| terminator.successors())
        .unwrap_or_default()
}

/// A pure value-producing op whose every virtual def is unused. Nested regions,
/// a terminator, a memory write, or any physical-register write keep it; an op
/// with SSA results must additionally declare pure semantics, so effectful ops
/// like calls survive even when their result is unread.
fn is_erasable(instance: &OpHandle, use_counts: &HashMap<u32, usize>) -> bool {
    if !instance.regions().is_empty()
        || instance.clone().as_interface::<dyn Terminator>().is_some()
        || instance.clone().as_interface::<dyn MemoryWrite>().is_some()
    {
        return false;
    }
    if !instance.results().is_empty() && !super::is_pure_value(instance) {
        return false;
    }

    let regs = op_regs(instance);
    if regs
        .defs
        .iter()
        .any(|r| matches!(r, RegRef::Physical { .. }))
    {
        return false;
    }

    let mut defines = false;
    for def in &regs.defs {
        if let RegRef::Virtual { id, .. } = def {
            defines = true;
            if use_counts.get(id).is_some_and(|&count| count > 0) {
                return false;
            }
        }
    }
    // Only a value-producing op is a DCE candidate; a def-less pure op is left alone.
    defines
}
