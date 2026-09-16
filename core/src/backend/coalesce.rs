//! Pre-allocation coalescing.
//!
//! Pre-allocation lowerings hand allocation a function full of copies: every
//! block-argument edge, every ABI boundary, every two-address tie became a
//! `copy` of one virtual register to another. Each such copy asks for one
//! register at both ends — but the request is only a hint to the solver, and
//! every ungranted one survives as a real instruction and keeps two live
//! ranges where one value flows.
//!
//! This pass merges the pairs outright. A copy's ends can share one value
//! whenever nothing separates them: liveness already records every point
//! where the two diverge (a redefinition of either while the other is live)
//! as interference, excluding the copy itself, so a non-interfering pair is
//! one whose live ranges never overlap with different values. Merging renames
//! one end onto the other — uses and definitions alike — and erases the
//! copies the rename turned into self-moves. Pressure never grows: at every
//! point at most one of the pair was live. Pinned values (ABI pins, fixed
//! registers) stay out: merging would drag the pin's constraints across a
//! range that never asked for them.

use std::collections::HashSet;

use tir::Terminator;
use tir::backend::regalloc::copy_endpoints;
use tir::{AnalysisManager, BlockId, Context, OperationRef, Pass, PassError, PassTarget, ValueId};

use crate::backend::liveness;
use crate::backend::prealloc::COALESCABLE_COPY_ATTR;
use crate::backend::regalloc::op_ref_in;
use crate::backend::registers::{RegAssignment, RegSlot, value_class};
use crate::backend::{ARG_PINS_ATTR, PINS_ATTR, SymbolOp, symbol_body_blocks};

/// Merge copy-connected vregs that do not interfere into one.
#[derive(Clone, Default)]
pub struct CoalescePass;

impl CoalescePass {
    pub fn new() -> Self {
        Self
    }
}

impl Pass for CoalescePass {
    fn name(&self) -> &'static str {
        "coalesce"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<SymbolOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let blocks = symbol_body_blocks(context, op.op());
        loop {
            let pinned = pinned_vregs(context, &blocks, op.op());
            let liveness = liveness::analyze(context, &blocks, |b| successors(context, &blocks, b));

            let mut merged = false;
            'blocks: for &block_id in &blocks {
                for op_id in context.get_block(block_id).op_ids().to_vec() {
                    if !context.has_operation(op_id) {
                        continue;
                    }
                    let copy = context.get_op(op_id);
                    if copy.attr(COALESCABLE_COPY_ATTR).is_none() {
                        continue;
                    }
                    let Some((src, dst)) = copy_endpoints(context, op_id) else {
                        continue;
                    };
                    if src == dst || pinned.contains(&src) || pinned.contains(&dst) {
                        continue;
                    }
                    let source = ValueId::from_number(src);
                    let target = ValueId::from_number(dst);
                    // An alloca result is no ordinary value: allocation
                    // erases the op and rematerializes every address. A merge
                    // that named it would leave the erasure owning a value
                    // other ops still define.
                    if is_alloca_result(context, &blocks, source)
                        || is_alloca_result(context, &blocks, target)
                    {
                        continue;
                    }
                    if value_class(context, source) != value_class(context, target)
                        || liveness.class_conflicts.contains_key(&dst)
                        || liveness.class_conflicts.contains_key(&src)
                        || liveness.interferes(dst, src)
                    {
                        continue;
                    }
                    // A block argument's parameter occurrence cannot be
                    // renamed, so the other end comes to it; with two free
                    // ends the destination folds into the source: the
                    // destination's reads all follow the copy, where the two
                    // hold one value.
                    let (old, new) = match (
                        context.is_block_argument(source),
                        context.is_block_argument(target),
                    ) {
                        (true, true) => continue,
                        (false, true) => (source, target),
                        _ => (target, source),
                    };
                    if !merge_keeps_block_order(context, &blocks, old, new) {
                        continue;
                    }
                    rename(context, &blocks, old, new);
                    erase_self_copies(context, &blocks)?;
                    merged = true;
                    break 'blocks;
                }
            }
            if !merged {
                return Ok(());
            }
        }
    }
}

/// Whether merging `old` into `new` keeps every use ahead of the value's last
/// definition in each block — the block-order invariant the verifier enforces.
/// Copies between the pair become self-moves and are erased by the merge, so
/// they count neither as uses nor as definitions here. A tied read at the
/// last definition reads itself and is fine; anything earlier is not.
fn merge_keeps_block_order(
    context: &Context,
    blocks: &[BlockId],
    old: ValueId,
    new: ValueId,
) -> bool {
    for block in blocks {
        let mut last_def = None;
        let mut uses = Vec::new();
        for (index, op_id) in context.get_block(*block).op_ids().into_iter().enumerate() {
            let op = context.get_op(op_id);
            let defines = op.results().contains(&old) || op.results().contains(&new);
            let reads = op.operands().contains(&old) || op.operands().contains(&new);
            if reads && defines && op.attr(COALESCABLE_COPY_ATTR).is_some() {
                continue;
            }
            if defines {
                last_def = Some(index);
            }
            if reads {
                uses.push(index);
            }
        }
        if let Some(last_def) = last_def
            && uses.iter().any(|&index| index < last_def)
        {
            return false;
        }
    }
    true
}

/// Retarget every read of `old` to `new`, and every definition: after the
/// rename only `new` names the merged value.
fn rename(context: &Context, blocks: &[BlockId], old: ValueId, new: ValueId) {
    context.replace_value_uses(old, new);
    for block in blocks {
        for op_id in context.get_block(*block).op_ids() {
            let op = context.get_op(op_id);
            for (index, result) in op.results().iter().enumerate() {
                if *result == old {
                    context.set_op_result(op_id, index, new);
                }
            }
        }
    }
}

/// Drop the copies the merge made circular: a copy from a value onto itself
/// moves nothing.
fn erase_self_copies(context: &Context, blocks: &[BlockId]) -> Result<(), PassError> {
    for block in blocks {
        for op_id in context.get_block(*block).op_ids().to_vec() {
            if !context.has_operation(op_id) {
                continue;
            }
            let op = context.get_op(op_id);
            if op.attr(COALESCABLE_COPY_ATTR).is_none() {
                continue;
            }
            if matches!(copy_endpoints(context, op_id), Some((src, dst)) if src == dst) {
                context.erase_op_keeping_results(&op_ref_in(context, op_id))?;
            }
        }
    }
    Ok(())
}

/// Whether any op in the body defines `value` by an `alloca`.
fn is_alloca_result(context: &Context, blocks: &[BlockId], value: ValueId) -> bool {
    blocks.iter().any(|block| {
        context.get_block(*block).op_ids().iter().any(|&op_id| {
            let op = context.get_op(op_id);
            op.clone().as_op::<tir::ptr::AllocaOp>().is_some() && op.results().contains(&value)
        })
    })
}

/// The vregs every pass and copy marks off-limits: the pinned fixed-register
/// slots still unlowered into point copies, and the incoming argument values
/// the calling convention pins at the boundary.
fn pinned_vregs(context: &Context, blocks: &[BlockId], symbol: &tir::OpHandle) -> HashSet<u32> {
    let mut pinned = HashSet::new();
    for block in blocks {
        for op_id in context.get_block(*block).op_ids() {
            let op = context.get_op(op_id);
            if op.attr(PINS_ATTR).is_none() {
                continue;
            }
            for slot in tir::backend::reg_slots(&op) {
                if let RegSlot::Value(value) = slot.slot {
                    pinned.insert(value.number());
                }
            }
        }
    }
    for (value, _) in RegAssignment::of_op(symbol, ARG_PINS_ATTR).iter() {
        pinned.insert(value.number());
    }
    pinned
}

fn successors(context: &Context, _blocks: &[BlockId], block: BlockId) -> Vec<BlockId> {
    let mut result = Vec::new();
    for op_id in context.get_block(block).op_ids() {
        let op = context.get_op(op_id);
        if let Some(term) = op.as_interface::<dyn Terminator>() {
            for succ in term.successors() {
                if !result.contains(&succ) {
                    result.push(succ);
                }
            }
        }
    }
    result
}
