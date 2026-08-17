//! Memory state erasure.
//!
//! The inverse of [threading](super::thread_state): every port a chain grew is
//! dropped and the states it named go with it, leaving the implicit memory order
//! the IR started from. State exists only between the two passes, so a function
//! that carries none is left alone.

use crate::analysis::slots::{collect_slots, load_result};
use crate::analysis::{AnalysisManager, DefUse};
use crate::builtin::StateType;
use crate::func::FuncOp;
use crate::state::EntryStateOp;
use crate::{
    BlockId, Context, MemoryRead, OpId, OperationRef, Pass, PassError, PassTarget, RegionId,
    Rewriter, TypeId,
};

#[derive(Default)]
pub struct EraseStatePass;

impl EraseStatePass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(EraseStatePass, "erase-state");

impl Pass for EraseStatePass {
    fn name(&self) -> &'static str {
        "erase-state"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        let state = StateType::new(context);
        let blocks = blocks_under(context, body);
        // Read off the threaded chain, before it is dropped.
        let unobserved = unobserved_stores(context, &blocks);

        // The states an op consumes go first, so nothing reads a definition by the
        // time the definition is dropped.
        for &block in &blocks {
            for op_id in context.get_block(block).op_ids() {
                drop_trailing_state_operands(context, op_id, state);
            }
        }

        let mut roots = Vec::new();
        for &block in &blocks {
            for op_id in context.get_block(block).op_ids() {
                if context.get_op(op_id).is::<EntryStateOp>() {
                    roots.push((block, op_id));
                    continue;
                }
                drop_trailing_state_results(context, op_id, state);
            }
            drop_trailing_state_arguments(context, block, state);
        }

        for (block, op_id) in roots {
            let block = context.get_block(block);
            rewriter.erase_op(&OperationRef::new(context.get_op(op_id), Some(block), None))?;
        }

        for store in unobserved {
            let block = context.parent_block(store).map(|id| context.get_block(id));
            rewriter.erase_op(&OperationRef::new(context.get_op(store), block, None))?;
        }

        sweep_dead_slots(context, rewriter, op.op().id, &blocks)
    }
}

/// The stores no read of their slot can observe. A slot whose address never
/// leaves the function is a memory of its own, so a write nothing reads back is
/// a write nobody can tell happened — the same reasoning the sweep below makes
/// about a slot with no read at all, taken one read at a time.
///
/// A read observes what was written when the chain it takes reaches the
/// allocation through anything but reads: a read leaves memory as it found it,
/// so one whose state came straight from the allocation, however many reads
/// apart, is reading what was never written. Anything else the chain names — a
/// write, a call, a port — is taken as an observation.
fn unobserved_stores(context: &Context, blocks: &[BlockId]) -> Vec<OpId> {
    let ops = blocks
        .iter()
        .flat_map(|&block| context.get_block(block).op_ids())
        .collect::<Vec<_>>();
    collect_slots(context, &ops)
        .into_values()
        .filter(|slot| !slot.escapes)
        .filter_map(|slot| {
            let alloca = slot.alloca?;
            slot.loads
                .iter()
                .all(|&load| reads_the_untouched_slot(context, load, alloca))
                .then_some(slot.stores)
        })
        .flatten()
        .collect()
}

/// Whether the state `load` reads comes from `alloca` through reads alone.
fn reads_the_untouched_slot(context: &Context, load: OpId, alloca: OpId) -> bool {
    let mut op = load;
    loop {
        let state = context
            .get_op(op)
            .as_interface::<dyn MemoryRead>()
            .and_then(|read| read.state_operand());
        let Some(defining) = state.and_then(|state| context.get_value(state).defining_op()) else {
            return false;
        };
        if defining == alloca {
            return true;
        }
        op = defining;
    }
}

/// Erase the slots nothing reads. A slot whose address never leaves the function
/// is a memory of its own, so with no read left there is nothing to observe what
/// its stores left behind and they die with the allocation. A load whose value
/// nothing takes is no read either — forwarding answers a load by its value and
/// leaves the operation standing — so it goes the same way.
///
/// Erasing state is what makes the sweep possible: while the chain is threaded,
/// the stores define the states their successors observe, and a load defines a
/// state of its own however unread its value is.
fn sweep_dead_slots(
    context: &Context,
    rewriter: &mut Rewriter,
    func: OpId,
    blocks: &[BlockId],
) -> Result<(), PassError> {
    let ops = blocks
        .iter()
        .flat_map(|&block| context.get_block(block).op_ids())
        .collect::<Vec<_>>();
    let mut uses: Option<DefUse> = None;
    for slot in collect_slots(context, &ops).into_values() {
        let Some(alloca) = slot.alloca else { continue };
        if slot.escapes {
            continue;
        }
        let index = uses.get_or_insert_with(|| DefUse::new(context, func));
        if slot
            .loads
            .iter()
            .any(|&load| index.is_used(load_result(context, load).number()))
        {
            continue;
        }
        for op_id in slot.loads.into_iter().chain(slot.stores).chain([alloca]) {
            let block = context.parent_block(op_id).map(|id| context.get_block(id));
            rewriter.erase_op(&OperationRef::new(context.get_op(op_id), block, None))?;
        }
    }
    Ok(())
}

fn drop_trailing_state_operands(context: &Context, op: OpId, state: TypeId) {
    while context
        .get_op(op)
        .operands()
        .last()
        .is_some_and(|&operand| context.get_value(operand).ty() == state)
    {
        context.pop_operand(op);
    }
}

fn drop_trailing_state_results(context: &Context, op: OpId, state: TypeId) {
    while context
        .get_op(op)
        .results()
        .last()
        .is_some_and(|&result| context.get_value(result).ty() == state)
    {
        context.pop_result(op);
    }
}

fn drop_trailing_state_arguments(context: &Context, block: BlockId, state: TypeId) {
    while context
        .get_block(block)
        .arguments()
        .last()
        .is_some_and(|argument| argument.ty() == state)
    {
        let index = context.get_block(block).arguments().len() - 1;
        context.remove_block_argument(block, index);
    }
}

/// Every block in a region tree, innermost first.
fn blocks_under(context: &Context, region: RegionId) -> Vec<BlockId> {
    let mut blocks = Vec::new();
    for block in context.get_region(region).iter(context.clone()) {
        for op_id in block.op_ids() {
            for nested in context.get_op(op_id).regions() {
                blocks.extend(blocks_under(context, nested));
            }
        }
        blocks.push(block.id());
    }
    blocks
}
