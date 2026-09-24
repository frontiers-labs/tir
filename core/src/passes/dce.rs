//! Dead code elimination shared by SSA functions and machine symbols: a
//! worklist over [`DefUse`] chains erases pure ops whose every result is
//! unused, retiring the erased op's reads so newly dead producers are revisited
//! without rescanning.
//!
//! In backend pipelines it must run before register allocation.
//! An explicit physical-register write counts as a side effect. Implicit writes
//! may disappear when their readers die and a later write replaces them.

use crate::analysis::{DefUse, execution_regs, op_regs};
use crate::backend::SymbolOp;
use std::collections::{HashMap, HashSet};

use crate::{
    AnalysisManager, Context, MemoryWrite, OpHandle, OpId, OperationRef, Pass, PassError,
    PassTarget, RegionId, Terminator, ValueId, func::FuncOp,
};

#[derive(Clone, Default)]
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
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() && op.as_op::<SymbolOp>().is_none() {
            return Ok(());
        }

        let defuse = analyses.get::<DefUse>(context, op.op().id);
        erase_dead_with(context, &defuse, op.op().id)
    }
}

/// A gate or a loop goes with the rest once nothing under it can be told to
/// have happened. The results of the regions under `root` sit in no use list,
/// so what they name is read here and renamed here.
fn erase_dead_with(context: &Context, defuse: &DefUse, root: OpId) -> Result<(), PassError> {
    // Renames walk from the root's own regions: those outlive every erase here,
    // while a region nested under an erased gate is gone by the time a later
    // read hands over its state.
    let roots = context.get_op(root).regions();
    let regions: Vec<RegionId> = super::regions_under(context, root);
    let mut named: HashSet<ValueId> = regions
        .iter()
        .flat_map(|&region| context.get_region(region).results())
        .collect();
    // Reuse the machine dependence scan to keep every final register write,
    // while allowing writes overwritten in the block once their readers die.
    let mut register_users = HashMap::new();
    let mut register_producers: HashMap<OpId, Vec<OpId>> = HashMap::new();
    for &region in &regions {
        for block in context.get_region(region).block_ids() {
            let ops = context.get_block(block).op_ids();
            let graph = crate::backend::Dependences::of_ops(
                context,
                &ops,
                &crate::backend::RegAssignment::default(),
            );
            for (index, &op) in ops.iter().enumerate() {
                if let Some(users) = graph.local_register_users(index) {
                    register_users.insert(op, users.to_vec());
                    for &user in users {
                        register_producers.entry(user).or_default().push(op);
                    }
                }
            }
        }
    }
    // LIFO over walk order visits consumers before their producers.
    let mut queue: Vec<OpId> = defuse.ops().to_vec();

    while let Some(op_id) = queue.pop() {
        if !context.has_operation(op_id) {
            continue;
        }
        let instance = context.get_op(op_id);
        let unused_registers = register_users
            .get(&op_id)
            .is_some_and(|users| users.iter().all(|&user| !context.has_operation(user)));
        if !is_erasable(context, &instance, &named, unused_registers) {
            continue;
        }

        // A read leaves memory as it found it, so the state it published names
        // the memory it observed: erasing the read hands its readers that one,
        // and the reads it hands over move to the state they now name — a write
        // whose state a forwarded reader took is still read.
        if let Some(effects) = instance
            .clone()
            .as_interface::<dyn crate::ResourceEffects>()
        {
            for effect in effects
                .resource_effects()
                .into_iter()
                .filter(|effect| effect.access == crate::ResourceAccess::Read)
            {
                for (published, observed) in effect.produced.iter().zip(&effect.observed) {
                    context.replace_value_uses(*published, *observed);
                    if named.remove(published) {
                        named.insert(*observed);
                        for &region in &roots {
                            context.rename_region_results(region, *published, *observed, &[]);
                        }
                    }
                }
            }
        }
        // Read before the erase: the op's storage goes away with it.
        let used_regs = op_regs(&instance).uses;
        context.erase_op(&OperationRef::new(instance.clone()))?;

        queue.extend(
            register_producers
                .get(&op_id)
                .into_iter()
                .flatten()
                .copied(),
        );

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
/// tell. Nested regions, a terminator, or an observable register write keep it; a
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
fn is_erasable(
    context: &Context,
    instance: &OpHandle,
    named: &HashSet<ValueId>,
    unused_registers: bool,
) -> bool {
    if instance.clone().as_interface::<dyn Terminator>().is_some() {
        return false;
    }
    // A gate or a loop is an ordinary def-use question once nothing under it can
    // be told to have happened: the arms a decided gate leaves behind are the
    // common case, and they are what every later pass would otherwise walk.
    let pure_regions = !instance.regions().is_empty() && pure_subtree(context, instance);
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
    let declared = instance
        .clone()
        .as_interface::<dyn crate::ResourceEffects>()
        .map(|effects| effects.resource_effects())
        .unwrap_or_default();
    let reads_only = !declared.is_empty()
        && declared
            .iter()
            .all(|effect| effect.access == crate::ResourceAccess::Read);
    let forwards_state = reads_only && !instance.state_operands().is_empty();
    let publishes_effects =
        !declared.is_empty() && declared.iter().all(|effect| !effect.produced.is_empty());
    // An allocation is the object its state names. With neither its address nor
    // that state read, the object is one nothing in the function can tell exists
    // — the slot sweep the chains make an ordinary def-use question.
    let allocation = instance.has_interface::<dyn crate::PromotableAllocation>();
    match &machine {
        Some(mi) if mi.info().effects.writes => return false,
        None if !instance.results().is_empty()
            && !publishes_effects
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

    if !op_regs(instance).phys_defs.is_empty() {
        return false;
    }
    let regs = execution_regs(instance);
    if !regs.phys_defs.is_empty() && !unused_registers {
        return false;
    }

    // A read hands its readers the dependency it took, so that result is not
    // one keeping it alive; every other dependency an op leaves is a definition
    // like its values.
    let published = instance.state_results();
    let mut defines = !regs.phys_defs.is_empty()
        && machine.as_ref().is_some_and(|mi| {
            use crate::backend::exec::{Dest, Effect, Program};
            let info = mi.info();
            info.effects == crate::backend::MemoryEffects::NONE
                && info.control_flow == crate::backend::ControlFlow::None
                && matches!(&info.program, Program::Effects { effects, .. }
                    if !effects.is_empty() && effects.iter().all(|effect| matches!(effect,
                        Effect::Assign { dest: Dest::Reg(_) | Dest::Fixed(_, _), .. })))
        });
    for def in regs
        .defs
        .iter()
        .chain(published.iter().filter(|_| !forwards_state))
    {
        defines = true;
        if context.is_used(*def) || named.contains(def) {
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
        crate::analysis::regions::region_ops(context, region)
            .into_iter()
            .all(|op| {
                let inner = context.get_op(op);
                inner.clone().as_interface::<dyn Terminator>().is_some()
                    || !inner.regions().is_empty()
                    || super::is_pure_value(&inner)
            })
    })
}
