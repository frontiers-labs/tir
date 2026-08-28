//! Dead code elimination shared by SSA functions and machine symbols: a
//! worklist over [`DefUse`] chains erases pure ops whose every result is
//! unused, retiring the erased op's reads so newly dead producers are revisited
//! without rescanning.
//!
//! In backend pipelines it must run before register allocation — a
//! physical-register write counts as a side effect, so nothing is eligible
//! after allocation.

use std::collections::HashMap;

use crate::analysis::{DefUse, execution_regs, op_regs};
use crate::backend::SymbolOp;
use crate::{
    AnalysisManager, Context, MemoryWrite, OpHandle, OpId, OperationRef, Pass, PassError,
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
        erase_dead(context, rewriter, &defuse)
    }
}

/// Erase every operation nothing can tell the absence of, cascading: an erased
/// op's operands whose def is then unread are revisited without rescanning.
///
/// The cascade is what a rewrite leaves behind, so
/// [`instcombine`](super::instcombine) runs it as part of its own commit; this
/// pass is the same walk where no rewrite caused it — after instruction
/// selection, which leaves values its fusions recomputed.
pub(crate) fn erase_dead(
    context: &Context,
    rewriter: &mut Rewriter,
    defuse: &DefUse,
) -> Result<(), PassError> {
    // Live read counts, retired as dead readers are erased.
    let mut use_counts = defuse.use_counts();
    // LIFO over walk order visits consumers before their producers.
    let mut queue: Vec<OpId> = defuse.ops().to_vec();

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
            let id = used.number();
            if let Some(count) = use_counts.get_mut(&id) {
                *count -= 1;
                if *count == 0 {
                    queue.extend_from_slice(defuse.defs_of(id));
                }
            }
        }
    }
    Ok(())
}

/// An op whose every virtual def is unused and whose absence nothing else can
/// tell. Nested regions, a terminator, or any physical-register write keep it; a
/// mid-end op with SSA results must additionally declare pure semantics, so
/// effectful ops like calls survive even when their result is unread.
///
/// A write to memory is the exception the state chains buy: what it publishes is
/// the whole of what anything can observe about it, so a write no state reads is
/// a write nobody can tell happened. Where no chain is threaded it publishes
/// nothing, defines nothing, and the last test below leaves it alone.
fn is_erasable(instance: &OpHandle, use_counts: &HashMap<u32, usize>) -> bool {
    if !instance.regions().is_empty() || instance.clone().as_interface::<dyn Terminator>().is_some()
    {
        return false;
    }
    // A machine instruction states its effects in its `InstrInfo`; the purity
    // declaration below is what a mid-end op has instead, and its results are
    // the only signal there.
    let machine = instance
        .clone()
        .as_interface::<dyn crate::backend::MachineInstruction>();
    let writes_memory = instance.has_interface::<dyn MemoryWrite>();
    match &machine {
        Some(mi) if mi.info().effects.writes => return false,
        None if !instance.results().is_empty()
            && !writes_memory
            && !super::is_pure_value(instance) =>
        {
            return false;
        }
        _ => {}
    }

    let regs = execution_regs(instance);
    if !regs.phys_defs.is_empty() {
        return false;
    }

    let mut defines = false;
    for def in &regs.defs {
        defines = true;
        if use_counts
            .get(&def.number())
            .is_some_and(|&count| count > 0)
        {
            return false;
        }
    }
    // Only a value-producing op is a DCE candidate; a def-less pure op is left alone.
    defines
}
