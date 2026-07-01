//! Whole-function DCE over machine IR: deletes pure ops whose every defined virtual register is unused,
//! to a fixpoint. Must run before regalloc — a physical-register write counts as a side effect, so nothing
//! is eligible after allocation.

use std::collections::HashSet;

use tir::{
    BlockId, Context, MemoryWrite, OpId, OperationRef, Pass, PassError, PassTarget, Rewriter,
    Terminator,
};

use crate::backend::liveness::{RegRef, op_regs};

#[derive(Default)]
pub struct DeadCodeEliminationPass;

impl DeadCodeEliminationPass {
    pub fn new() -> Self {
        Self
    }
}

impl Pass for DeadCodeEliminationPass {
    fn name(&self) -> &'static str {
        "dead-code-elimination"
    }

    fn target(&self) -> PassTarget {
        PassTarget::Operation("symbol")
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        let Some(&region) = op.op().regions.first() else {
            return Ok(());
        };
        let blocks: Vec<BlockId> = context
            .get_region(region)
            .iter(context.clone())
            .map(|b| b.id())
            .collect();

        // Iterate to a fixpoint: deleting a dead op can make its pure operands dead.
        loop {
            let live_ops: Vec<(BlockId, OpId)> = blocks
                .iter()
                .flat_map(|&b| {
                    context
                        .get_block(b)
                        .op_ids()
                        .into_iter()
                        .map(move |id| (b, id))
                })
                .collect();

            // Every virtual register read by a surviving op.
            let mut used: HashSet<u32> = HashSet::new();
            for &(_, op_id) in &live_ops {
                for r in op_regs(&context.get_op(op_id)).uses {
                    if let RegRef::Virtual { id, .. } = r {
                        used.insert(id);
                    }
                }
            }

            let dead: Vec<(BlockId, OpId)> = live_ops
                .into_iter()
                .filter(|&(_, op_id)| is_dead(context, op_id, &used))
                .collect();
            if dead.is_empty() {
                break;
            }
            for (block, op_id) in dead {
                let op_ref =
                    OperationRef::new(context.get_op(op_id), Some(context.get_block(block)), None);
                rewriter.erase_op(&op_ref)?;
            }
        }
        Ok(())
    }
}

/// A pure value-producing op whose every defined virtual register is unused; nested regions, a terminator,
/// a memory write, or any physical-register write keep it.
fn is_dead(context: &Context, op_id: OpId, used: &HashSet<u32>) -> bool {
    let instance = context.get_op(op_id);
    if !instance.regions.is_empty()
        || instance.clone().as_interface::<dyn Terminator>().is_some()
        || instance.clone().as_interface::<dyn MemoryWrite>().is_some()
    {
        return false;
    }

    let regs = op_regs(&instance);
    if regs
        .defs
        .iter()
        .any(|r| matches!(r, RegRef::Physical { .. }))
    {
        return false;
    }

    let mut defines = false;
    for r in &regs.defs {
        if let RegRef::Virtual { id, .. } = r {
            defines = true;
            if used.contains(id) {
                return false;
            }
        }
    }
    // Only a value-producing op is a DCE candidate; a def-less pure op is left alone.
    defines
}
