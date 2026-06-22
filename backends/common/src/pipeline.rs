//! The shared backend pass pipeline, used by `tir mc` and `fcc`.
//!
//! Ordering matters: `vcond_br` is lowered to a real conditional branch plus
//! `vbr` *before* register allocation because its condition is an SSA value
//! the allocator must color, while `vret`/`vbr` are finalized *after* it
//! because the allocator matches `vret` by name to precolor return values.

use tir::{Context, Operation, PassManager, builtin::FuncOp};

use crate::TargetMachine;
use crate::lower::OpLoweringPass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAfter {
    ISel,
    RegAlloc,
    Finalize,
}

/// Build the lowering pipeline for `target`: instruction selection, pre-RA
/// lowerings, register allocation, and post-RA finalization.
pub fn build_pipeline(
    target: &dyn TargetMachine,
    context: &Context,
    stop: StopAfter,
) -> PassManager {
    let mut pm = PassManager::new();
    // Target-provided pre-selection lowerings (e.g. splitting two-way branches
    // into `asm.condbr` + `br` so the condition is covered by the e-graph).
    let pre_isel = target.pre_isel_lowerings();
    if !pre_isel.is_empty() {
        pm.add_pass(OpLoweringPass::new("pre-isel-lowering", pre_isel));
    }
    pm.nest(FuncOp::name()).add_pass(target.isel_pass(context));
    if stop == StopAfter::ISel {
        return pm;
    }

    let pre_ra = target.pre_ra_lowerings();
    if !pre_ra.is_empty() {
        pm.add_pass(OpLoweringPass::new("pre-ra-lowering", pre_ra));
    }
    pm.add_pass(target.regalloc_pass());
    if stop == StopAfter::RegAlloc {
        return pm;
    }

    let finalize = target.finalize_lowerings();
    if !finalize.is_empty() {
        pm.add_pass(OpLoweringPass::new("finalize-lowering", finalize));
    }
    pm
}
