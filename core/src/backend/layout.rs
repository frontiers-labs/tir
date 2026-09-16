//! Block layout.
//!
//! Selection lowers every branch to an unconditional branch plus, on the
//! other edge, a conditional branch, and destructuring mints standalone
//! reshape blocks. The destructure order is layout, so many conditional
//! branches end up jumping on to the block that already follows them, and
//! hopping through blocks that hold nothing but their own forward. Each such
//! jump is a five-byte instruction the object encoder fills with a zero
//! displacement, and each reshape block is a label nothing reads.
//!
//! This pass elides a trailing branch whose target is the physically next
//! block — control falls through anyway — and retargets and drops the blocks
//! whose content the elision emptied. It runs after allocation, against
//! block references (offsets fix up at emission), so the change touches the
//! layout shape and nothing else.

use tir::attributes::AttributeValue;
use tir::backend::VirtualBranchOp;
use tir::backend::regalloc::op_ref_in;
use tir::{AnalysisManager, BlockId, Context, OperationRef, Pass, PassError, PassTarget};

use crate::backend::{SymbolOp, symbol_body_blocks};

/// Elide branches to the next block, and the blocks whose content that
/// elision emptied.
#[derive(Clone, Default)]
pub struct LayoutPass;

impl LayoutPass {
    pub fn new() -> Self {
        Self
    }
}

impl Pass for LayoutPass {
    fn name(&self) -> &'static str {
        "layout"
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
        let mut blocks = symbol_body_blocks(context, op.op());
        loop {
            let mut changed = false;

            // Elide a trailing branch whose target is the block that follows
            // in layout order: on that edge control falls through anyway.
            for (position, &block_id) in blocks.iter().enumerate() {
                let Some(next) = blocks.get(position + 1).copied() else {
                    continue;
                };
                let op_ids = context.get_block(block_id).op_ids().to_vec();
                // The entry block's list ends with `symbol_end`; the branch
                // ahead of it is the one that may fall through.
                let last_op_id = op_ids.iter().rev().copied().find(|&op_id| {
                    context.has_operation(op_id)
                        && !context.get_op(op_id).is::<crate::backend::SymbolEndOp>()
                });
                let Some(last_op_id) = last_op_id else {
                    continue;
                };
                let last_op = context.get_op(last_op_id);
                if !last_op.is::<VirtualBranchOp>() {
                    continue;
                }
                let Some(AttributeValue::Block(dest)) = last_op.attr("dest") else {
                    continue;
                };

                if dest != next || !last_op.operands().is_empty() {
                    continue;
                }
                context.erase_op(&op_ref_in(context, last_op_id))?;
                changed = true;
            }

            // A block the elision emptied forwards what reached it on to the
            // block that follows, then drops out. The entry never drops: it
            // is the function's start.
            for position in 1..blocks.len() {
                let block_id = blocks[position];
                let Some(next) = blocks.get(position + 1).copied() else {
                    continue;
                };
                if !context.get_block(block_id).op_ids().is_empty() {
                    continue;
                }
                retarget(context, &blocks, block_id, next);
                context.erase_block(block_id);
                changed = true;
            }

            if !changed {
                return Ok(());
            }
            blocks = symbol_body_blocks(context, op.op());
            if blocks.is_empty() {
                return Ok(());
            }
        }
    }
}

/// Point every branch away from `gone` at `into`. Both branches and the
/// conditional-jump instructions name their target through a block
/// attribute; nothing else in machine IR names a block.
fn retarget(context: &Context, blocks: &[BlockId], gone: BlockId, target: BlockId) {
    let replacement = AttributeValue::Block(target);
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            if !context.has_operation(op_id) {
                continue;
            }
            let mut ops = context.get_op(op_id).attributes().to_vec();
            let mut rewritten = false;
            for attr in ops.iter_mut() {
                if attr.value == AttributeValue::Block(gone) {
                    attr.value = replacement.clone();
                    rewritten = true;
                }
            }
            if rewritten {
                context.set_op_attributes(op_id, ops);
            }
        }
    }
}
