//! Dead code elimination shared by SSA functions and machine symbols: a
//! worklist over [`DefUse`] chains erases pure ops whose every result is
//! unused, retiring the erased op's reads so newly dead producers are revisited
//! without rescanning.
//!
//! In backend pipelines it must run before register allocation — a
//! physical-register write counts as a side effect, so nothing is eligible
//! after allocation.

use crate::analysis::{DefUse, execution_regs, op_regs};
use crate::backend::SymbolOp;
use crate::builtin::{StateType, trailing_state_operand, trailing_state_result};
use crate::{
    AnalysisManager, Context, MemoryWrite, OpHandle, OpId, OperationRef, Pass, PassError,
    PassTarget, Rewriter, Terminator, ValueId, func::FuncOp,
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
        erase_dead_with(context, rewriter, &defuse, true)
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
    erase_dead_with(context, rewriter, defuse, false)
}

/// `regions` admits a gate or a loop nothing under which can be told to have
/// happened. A rewrite's own commit leaves that alone: the control it orphaned
/// is still the shape the rewrite was read against, and removing it is this
/// pass's sweep rather than the rewriter's.
fn erase_dead_with(
    context: &Context,
    rewriter: &mut Rewriter,
    defuse: &DefUse,
    regions: bool,
) -> Result<(), PassError> {
    // LIFO over walk order visits consumers before their producers.
    let mut queue: Vec<OpId> = defuse.ops().to_vec();

    while let Some(op_id) = queue.pop() {
        if !context.has_operation(op_id) {
            continue;
        }
        let instance = context.get_op(op_id);
        if !is_erasable(context, &instance, regions) {
            continue;
        }

        // A read leaves memory as it found it, so the state it published names
        // the memory it observed: erasing the read hands its readers that one,
        // and the reads it hands over move to the state they now name — a write
        // whose state a forwarded reader took is still read.
        if let (Some(published), Some(observed)) = (
            trailing_state_result(context, &instance),
            trailing_state_operand(context, &instance),
        ) {
            context.replace_value_uses(published, observed);
        }
        // Read before the erase: the op's storage goes away with it.
        let used_regs = op_regs(&instance).uses;
        rewriter.erase_op(&OperationRef::new(instance.clone()))?;

        // The erase retired the op's own reads, so a value it held alone is
        // now unread and its producers are candidates in turn.
        for used in used_regs {
            if !context.is_used(used) {
                queue.extend_from_slice(defuse.defs_of(used.number()));
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
/// Memory accesses are the exception the state chains buy. What a write
/// publishes is the whole of what anything can observe about it, so a write no
/// state reads is a write nobody can tell happened; a read leaves memory as it
/// found it, so one whose value nothing takes is a read nobody can tell happened
/// either, and its readers are handed the state it observed. Where no chain is
/// threaded a write publishes nothing, defines nothing, and the last test below
/// leaves it alone.
fn is_erasable(context: &Context, instance: &OpHandle, regions: bool) -> bool {
    if instance.clone().as_interface::<dyn Terminator>().is_some() {
        return false;
    }
    // A gate or a loop is an ordinary def-use question once nothing under it can
    // be told to have happened: the arms a decided gate leaves behind are the
    // common case, and they are what every later pass would otherwise walk.
    let pure_regions = regions && !instance.regions().is_empty() && pure_subtree(context, instance);
    if !instance.regions().is_empty() && !pure_regions {
        return false;
    }
    // A machine instruction states its effects in its `InstrInfo`; the purity
    // declaration below is what a mid-end op has instead, and its results are
    // the only signal there.
    let machine = instance
        .clone()
        .as_interface::<dyn crate::backend::MachineInstruction>();
    let writes_memory = instance.has_interface::<dyn MemoryWrite>();
    // A read leaves memory as it found it, so the state it publishes names the
    // one it observed: its state result is not a definition that keeps it alive,
    // and erasing it hands its readers the state it took. Read off the declared
    // effects, not off the absence of a write interface — a call writes memory
    // and declares no location for it.
    let reads_only = match &machine {
        Some(mi) => mi.info().effects.reads && !mi.info().effects.writes,
        None => instance.has_interface::<dyn crate::MemoryRead>() && !writes_memory,
    };
    let forwards_state = reads_only && trailing_state_operand(context, instance).is_some();
    // An allocation is the object its state names. With neither its address nor
    // that state read, the object is one nothing in the function can tell exists
    // — the slot sweep the chains make an ordinary def-use question.
    let allocation = instance.has_interface::<dyn crate::PromotableAllocation>();
    match &machine {
        Some(mi) if mi.info().effects.writes => return false,
        None if !instance.results().is_empty()
            && !writes_memory
            && !forwards_state
            && !allocation
            && !pure_regions
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

    let state = StateType::new(context);
    let is_state =
        |value: &ValueId| context.has_value(*value) && context.get_value(*value).ty() == state;
    let mut defines = false;
    for def in regs
        .defs
        .iter()
        .filter(|def| !(forwards_state && is_state(def)))
    {
        defines = true;
        if context.is_used(*def) {
            return false;
        }
    }
    // Only a value-producing op is a DCE candidate; a def-less pure op is left alone.
    defines
}

/// Whether nothing under `instance`'s regions can be told to have happened: no
/// access, no call, nothing holding a physical register. A nested region op is
/// read through, since everything it holds is in the same walk.
fn pure_subtree(context: &Context, instance: &OpHandle) -> bool {
    instance.regions().iter().all(|&region| {
        crate::analysis::scopes::region_ops(context, region)
            .into_iter()
            .all(|op| {
                let inner = context.get_op(op);
                inner.clone().as_interface::<dyn Terminator>().is_some()
                    || !inner.regions().is_empty()
                    || super::is_pure_value(&inner)
            })
    })
}
