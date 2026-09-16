//! Constant rematerialization.
//!
//! Selection may materialize the same integer constant several times, each a
//! distinct virtual register live from its definition to its last use. At
//! allocation those registers are pure overhead: nothing was computed, so a
//! spilled one needs no stack slot — only the instruction that recomputes it,
//! replayed at each site that reads it. This answers which vregs those are
//! keyed to the definition that materializes them; the allocator's spill
//! rewrite re-emits that instruction per use instead of a store and a reload,
//! and prices the spill as one extra instruction per use.

use std::collections::HashMap;

use tir::attributes::AttributeValue;
use tir::backend::{ControlFlow, MachineInstruction, MemoryEffects};
use tir::{BlockId, Context, OpId};

use crate::backend::liveness::Liveness;
use crate::backend::reg_slots;
use crate::backend::registers::{RegSlot, value_class};

/// The spill cost the allocator prices a rematerializable vreg at: at most one
/// extra instruction per use and no memory chain, against the two accesses a
/// real spill pays for its store and each reload.
pub(crate) const REMAT_SPILL_USE_COST: u64 = 3;

/// Which vregs stand for immediate constants, keyed to the definition that
/// materializes them: the values the allocator may drop at spill time in
/// favor of re-issuing the definition at each use. A vreg referenced through
/// any class other than the one its definition writes does not qualify: the
/// replay would produce a value the instructions reading it cannot honor. A
/// vreg defined more than once does not qualify either: the replay retires
/// the constant definition, and a second definition — a two-address op
/// rewriting the value — would read a register nothing stands behind anymore.
pub(crate) fn rematerializable(
    context: &Context,
    blocks: &[BlockId],
    liveness: &Liveness,
) -> HashMap<u32, OpId> {
    let mut def_counts: HashMap<u32, usize> = HashMap::new();
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            for result in context.get_op(op_id).results() {
                *def_counts.entry(result.number()).or_default() += 1;
            }
        }
    }
    let mut defs: HashMap<u32, OpId> = HashMap::new();
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            let op = context.get_op(op_id);
            let Some(mi) = op.clone().as_interface::<dyn MachineInstruction>() else {
                continue;
            };
            let info = mi.info();
            if info.control_flow != ControlFlow::None
                || !info.implicit_regs.is_empty()
                || info.effects != MemoryEffects::NONE
            {
                continue;
            }
            let slots = reg_slots(&op);
            let [slot] = slots.as_slice() else {
                continue;
            };
            let RegSlot::Value(result) = slot.slot else {
                continue;
            };
            if !slot.port.def
                || !op.value_operands().is_empty()
                || !op.state_operands().is_empty()
                || !op.state_results().is_empty()
                || def_counts.get(&result.number()).copied().unwrap_or(0) != 1
            {
                continue;
            }
            if !matches!(op.attr("imm"), Some(AttributeValue::Int(_))) {
                continue;
            }
            if liveness.vreg_class.get(&result.number()).copied() != value_class(context, result) {
                continue;
            }
            defs.entry(result.number()).or_insert(op_id);
        }
    }
    defs
}
