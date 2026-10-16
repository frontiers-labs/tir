//! Pre-allocation lowering: after instruction selection and before register
//! allocation, tied (two-address) operands become explicit copies.
//! [`regalloc_stage_for`] composes that pass with the allocation pass.

use tir::attributes::AttributeValue;
use tir::{AnalysisManager, Context, OperationRef, Pass, PassError, PassTarget};

use crate::backend::regalloc::{RegisterAllocationPass, TargetRegAlloc, op_ref_in};
use crate::backend::registers::{RegSlot, value_class};
use crate::backend::{SymbolOp, reg_slots, symbol_body_blocks};

/// Compose the register-allocation stage for a target: tied-operand lowering
/// followed by the allocation pass. `make_target` is called once per pass
/// because each pass owns its target hooks.
pub fn regalloc_stage_for(
    make_target: impl Fn() -> Box<dyn TargetRegAlloc>,
    abi: &'static crate::backend::abi::AbiInfo,
) -> Vec<Box<dyn tir::Pass>> {
    vec![
        Box::new(TiedOperandLoweringPass::new(make_target())),
        Box::new(RegisterAllocationPass::with_abi(make_target(), abi)),
    ]
}

/// Lower tied (two-address) operands ahead of allocation. An instruction whose
/// behavior reads its destination (the x86 `dst = dst + src`) declares its
/// source slot tied to its destination slot: it defines `%r` but must read `%x`
/// through the same register. Insert `copy %r <- %x` ahead of the op and point
/// the tied slot at `%r`, leaving a plain read-modify-write of `%r` that
/// liveness and coloring already model. The copy carries
/// `COALESCABLE_COPY_ATTR`: the tie is a request for one register at both
/// ends, so allocation reads it as an affinity and erases the copy where it is
/// granted.
pub struct TiedOperandLoweringPass {
    target: Box<dyn TargetRegAlloc>,
}

impl TiedOperandLoweringPass {
    pub fn new(target: Box<dyn TargetRegAlloc>) -> Self {
        Self { target }
    }
}

impl Pass for TiedOperandLoweringPass {
    fn name(&self) -> &'static str {
        "tied-operand-lowering"
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
        for block_id in symbol_body_blocks(context, op.op()) {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                let slots = reg_slots(&op);
                let mut ties = Vec::new();
                for slot in &slots {
                    let (Some(tied_to), RegSlot::Value(src), Some(position)) =
                        (slot.port.tied_to, slot.slot, slot.position)
                    else {
                        continue;
                    };
                    let Some(destination) = slots.iter().find(|s| s.port.name == tied_to) else {
                        return Err(PassError::InvalidRuleSet(format!(
                            "tied operand '{}' names no destination",
                            slot.port.name
                        )));
                    };
                    let RegSlot::Value(dst) = destination.slot else {
                        continue;
                    };
                    let class = value_class(context, dst).ok_or_else(|| {
                        PassError::InvalidRuleSet(format!(
                            "tied operand '{}' has no register class",
                            slot.port.name
                        ))
                    })?;
                    ties.push((position, dst, src, class));
                }
                if ties.is_empty() {
                    continue;
                }
                let op_ref = op_ref_in(context, op_id);
                for &(position, dst, src, class) in &ties {
                    let copy = self.target.emit_copy(
                        context,
                        class,
                        RegSlot::Value(dst),
                        RegSlot::Value(src),
                    );
                    let copy_id = copy.id();
                    context.insert_op_before(&op_ref, copy.as_ref())?;
                    mark_op(
                        context,
                        copy_id,
                        COALESCABLE_COPY_ATTR,
                        AttributeValue::Bool(true),
                    );
                    context.set_op_operand(op_id, position, dst);
                }
            }
        }
        Ok(())
    }
}

/// Marks a copy whose two ends want the same register: a two-address tie here,
/// a call operand in [`crate::backend::call_lowering`], an ABI boundary or a
/// fixed-register instruction operand in [`crate::backend::regalloc`]. Register allocation reads the
/// endpoints as a coalescing affinity, erases the copy when both land in one
/// register, and strips the mark.
pub(crate) const COALESCABLE_COPY_ATTR: &str = "coalescable_copy";

pub(crate) fn mark_op(context: &Context, op_id: tir::OpId, name: &str, value: AttributeValue) {
    let mut attrs = context.get_op(op_id).attributes().to_vec();
    attrs.push(context.named_attribute(name, value));
    context.set_op_attributes(op_id, attrs);
}
