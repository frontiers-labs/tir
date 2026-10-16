//! Shuffle code on the edges whose affinities the solve did not meet (paper
//! § 1): an `asm.vbr` passes its value arguments to its destination's
//! parameters as one parallel copy, sequenced here on the physical registers
//! the solve chose. A constant is replayed into its destination instead of
//! moved there.

use std::collections::{HashMap, HashSet};

use tir::attributes::AttributeValue;
use tir::{BlockId, Context, OpId, OperationRef, PassError, ValueId};

use super::db::{self, Named, RegAllocDb};
use super::{
    FramePlan, RegClassId, RegisterAllocationPass, SlotChain, TargetRegAlloc,
    allocation_copy_endpoints, allocation_order, op_ref_in,
};
use crate::backend::abi::{AbiInfo, value_kind};
use crate::backend::liveness::{PhysReg, op_regs};
use crate::backend::registers::{fresh_reg, value_class};
use crate::backend::{ParallelCopyOp, RegSlot, VirtualBranchOp};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Reg(PhysReg),
    Slot(usize),
    Const(OpId),
}

// Per value, by number, the definition of the constant it holds: the
// rematerializable values, the parallel copies of them, and the parameters
// every edge passes one of them.
pub(super) fn constants(
    context: &Context,
    blocks: &[BlockId],
    mut constants: HashMap<u32, OpId>,
) -> HashMap<u32, OpId> {
    let mut pairs: Vec<(ValueId, ValueId)> = Vec::new();
    let mut incoming: HashMap<ValueId, Vec<ValueId>> = HashMap::new();
    for &block in blocks {
        for op_id in context.get_block(block).op_ids() {
            let op = context.get_op(op_id);
            if op.is::<ParallelCopyOp>() {
                pairs.extend(
                    op.operands()
                        .iter()
                        .copied()
                        .zip(op.results().iter().copied()),
                );
            } else if op.is::<VirtualBranchOp>()
                && let Some(AttributeValue::Block(dest)) = op.attr("dest")
            {
                let params = context.get_block(dest).value_arguments();
                for (param, &arg) in params.iter().zip(op.value_operands().iter()) {
                    incoming.entry(param.id()).or_default().push(arg);
                }
            }
        }
    }
    // Optimistic, as constant propagation is: a parameter a loop passes back
    // its own copy holds the constant until an edge passes another value.
    // A value nothing below names is not a constant.
    let mut known: HashMap<ValueId, Option<OpId>> = HashMap::new();
    let derived: HashSet<ValueId> = pairs
        .iter()
        .map(|&(_, dst)| dst)
        .chain(incoming.keys().copied())
        .collect();
    let state = |known: &HashMap<ValueId, Option<OpId>>, value: ValueId| -> Option<Option<OpId>> {
        match constants.get(&value.number()) {
            Some(&def) => Some(Some(def)),
            None if derived.contains(&value) => known.get(&value).copied(),
            None => Some(None),
        }
    };
    let meet = |a: Option<Option<OpId>>, b: Option<Option<OpId>>| match (a, b) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) if a == b => Some(a),
        _ => Some(None),
    };
    let mut changed = true;
    while changed {
        changed = false;
        let mut update = |known: &mut HashMap<ValueId, Option<OpId>>, value, next| {
            if let Some(next) = next
                && known.get(&value) != Some(&next)
            {
                known.insert(value, next);
                changed = true;
            }
        };
        for &(src, dst) in &pairs {
            let next = meet(state(&known, dst), state(&known, src));
            update(&mut known, dst, next);
        }
        for (&param, args) in &incoming {
            let next = args.iter().fold(state(&known, param), |acc, &arg| {
                meet(acc, state(&known, arg))
            });
            update(&mut known, param, next);
        }
    }
    for (value, def) in known {
        if let Some(def) = def {
            constants.entry(value.number()).or_insert(def);
        }
    }
    constants
}

fn source(
    context: &Context,
    constants: &HashMap<u32, OpId>,
    src: ValueId,
    from: PhysReg,
    to: PhysReg,
) -> Source {
    match constants.get(&src.number()) {
        Some(&def) if replayable(context, def, to) => Source::Const(def),
        _ => Source::Reg(from),
    }
}

// A replay must find its definition standing and write the whole register: a
// view that merges into the rest of its register would depend on what the
// register held, where a move writes it whole.
pub(super) fn replayable(context: &Context, def: OpId, register: PhysReg) -> bool {
    context.has_operation(def)
        && context
            .get_op(def)
            .results()
            .first()
            .and_then(|&result| value_class(context, result))
            .is_some_and(|class| {
                class.file() == register.0.file()
                    && class.contains(register.1)
                    && !class.info().view.merge
            })
}

// Replace a copy left standing whose source holds a constant with a replay
// of the constant into the copy's destination. Reports whether it did.
pub(super) fn replay_constant_copy(
    context: &Context,
    constants: &HashMap<u32, OpId>,
    assignment: &HashMap<u32, PhysReg>,
    copy: OpId,
) -> Result<bool, PassError> {
    let Some((RegSlot::Value(src), RegSlot::Value(dst))) = allocation_copy_endpoints(context, copy)
    else {
        return Ok(false);
    };
    let (Some(&def), Some(&to)) = (constants.get(&src.number()), assignment.get(&dst.number()))
    else {
        return Ok(false);
    };
    if !replayable(context, def, to) {
        return Ok(false);
    }
    let (replay, fresh) = RegisterAllocationPass::rematerialize(context, def, src)?;
    let position = context
        .get_op(replay)
        .results()
        .iter()
        .position(|&result| result == fresh)
        .unwrap_or(0);
    context.set_op_result(replay, position, dst);
    let before = op_ref_in(context, copy);
    let replay = context.get_op(replay).as_dyn_op();
    context.insert_op_before(&before, replay.as_ref())?;
    context.erase_op_keeping_results(&before)?;
    Ok(true)
}

// Place the copies of every edge, then drop the branches' value arguments.
// A value the copies introduce is recorded in `assignment`.
#[allow(clippy::too_many_arguments)]
pub(super) fn place_edge_copies(
    context: &Context,
    target: &dyn TargetRegAlloc,
    abi: &AbiInfo,
    db: &RegAllocDb,
    constants: &HashMap<u32, OpId>,
    assignment: &mut HashMap<u32, PhysReg>,
    frame: &mut FramePlan,
) -> Result<(), PassError> {
    let info = target.register_info();
    let mut livein: HashMap<u32, Vec<u32>> = HashMap::new();
    let table = db.table(db::LIVEIN);
    for (&block, &value) in table.column(1).iter().zip(table.column(2)) {
        livein.entry(block).or_default().push(value);
    }
    for ops in &db.ops {
        for &op_id in ops {
            let op = context.get_op(op_id);
            if !op.is::<VirtualBranchOp>() || op.operands().is_empty() {
                continue;
            }
            let Some(AttributeValue::Block(dest)) = op.attr("dest") else {
                return Err(PassError::InvalidRuleSet(
                    "vbr with block arguments is missing its 'dest' target".to_string(),
                ));
            };
            let params = context.get_block(dest).value_arguments();
            let args = op.value_operands();
            let mut moves: Vec<(PhysReg, Source)> = Vec::new();
            // Every parameter's register is taken on entry, a parameter whose
            // argument already sits in it included.
            let mut held: Vec<PhysReg> = Vec::new();
            for (param, &arg) in params.iter().zip(args.iter()) {
                let param = param.id();
                let (Some(&dst), Some(&src)) = (
                    assignment.get(&param.number()),
                    assignment.get(&arg.number()),
                ) else {
                    continue;
                };
                held.push(dst);
                if param == arg || dst.0.span(dst.1) == src.0.span(src.1) {
                    continue;
                }
                // A copy moves the whole value, so its width follows the
                // parameter's type where that names the file.
                let class = info
                    .default_class(abi, value_kind(context, abi, param))
                    .filter(|class| class.file() == dst.0.file())
                    .unwrap_or(dst.0);
                let to = (class, dst.1);
                moves.push((to, source(context, constants, arg, (class, src.1), to)));
            }
            if !moves.is_empty() {
                let dest_id = db.block_id(dest);
                let mut live: Vec<PhysReg> = dest_id
                    .and_then(|block| livein.get(&block))
                    .into_iter()
                    .flatten()
                    .filter_map(|&value| match db.named(value) {
                        Named::Value(value) => assignment.get(&value.number()).copied(),
                        _ => None,
                    })
                    .collect();
                live.extend(held);
                let before = op_ref_in(context, op_id);
                emit_moves(
                    context, target, abi, &before, moves, &live, assignment, frame,
                )?;
            }
            context.set_op_operands(op_id, Vec::new());
        }
    }
    Ok(())
}

// Sequence the parallel copies that split live ranges before constrained
// instructions (paper § 4) on the assigned registers, then drop them. A
// value the copies introduce is recorded in `assignment`.
#[allow(clippy::too_many_arguments)]
pub(super) fn place_parallel_copies(
    context: &Context,
    target: &dyn TargetRegAlloc,
    abi: &AbiInfo,
    db: &RegAllocDb,
    constants: &HashMap<u32, OpId>,
    assignment: &mut HashMap<u32, PhysReg>,
    frame: &mut FramePlan,
) -> Result<(), PassError> {
    let info = target.register_info();
    let mut copies: HashMap<u32, OpId> = HashMap::new();
    for (ops, ids) in db.ops.iter().zip(&db.op_ids) {
        for (&op, &id) in ops.iter().zip(ids) {
            if context.get_op(op).is::<ParallelCopyOp>() {
                copies.insert(id, op);
            }
        }
    }
    let mut live: HashMap<u32, Vec<PhysReg>> = HashMap::new();
    let table = db.table(db::LIVE_AT);
    for (&op, &value) in table.column(1).iter().zip(table.column(2)) {
        if copies.contains_key(&op)
            && let Some(&register) = assignment.get(&db.value_of(value).number())
        {
            live.entry(op).or_default().push(register);
        }
    }
    for (id, op_id) in copies {
        let op = context.get_op(op_id);
        let mut moves: Vec<(PhysReg, Source)> = Vec::new();
        for (&src, &dst) in op.operands().iter().zip(op.results().iter()) {
            let (Some(&to), Some(&from)) =
                (assignment.get(&dst.number()), assignment.get(&src.number()))
            else {
                continue;
            };
            if to.0.span(to.1) == from.0.span(from.1) {
                continue;
            }
            let class = info
                .default_class(abi, value_kind(context, abi, dst))
                .filter(|class| class.file() == to.0.file())
                .unwrap_or(to.0);
            let to = (class, to.1);
            moves.push((to, source(context, constants, src, (class, from.1), to)));
        }
        let before = op_ref_in(context, op_id);
        let live = live.remove(&id).unwrap_or_default();
        emit_moves(
            context, target, abi, &before, moves, &live, assignment, frame,
        )?;
        context.erase_op_keeping_results(&before)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_moves(
    context: &Context,
    target: &dyn TargetRegAlloc,
    abi: &AbiInfo,
    before: &OperationRef,
    moves: Vec<(PhysReg, Source)>,
    live: &[PhysReg],
    assignment: &mut HashMap<u32, PhysReg>,
    frame: &mut FramePlan,
) -> Result<(), PassError> {
    let mut live = live.to_vec();
    live.extend(named_live(context, before.op().id, assignment));
    let steps = sequence(moves, &live, |class| scratch(abi, assignment, class));
    let mut slots: Vec<(i64, u32, SlotChain)> = Vec::new();
    for (dst, src) in steps {
        match src {
            Step::Move(src) => {
                let copy = target.emit_copy(context, dst.0, RegSlot::Phys(dst), RegSlot::Phys(src));
                context.insert_op_before(before, copy.as_ref())?;
            }
            Step::Replay(def) => {
                let constant = context.get_op(def).results()[0];
                let (replay, fresh) =
                    RegisterAllocationPass::rematerialize(context, def, constant)?;
                let class = value_class(context, fresh).unwrap_or(dst.0);
                assignment.insert(fresh.number(), (class, dst.1));
                let replay = context.get_op(replay).as_dyn_op();
                context.insert_op_before(before, replay.as_ref())?;
            }
            Step::Store(slot) => {
                // A slot reused for a wider class than it was sized for is
                // replaced: its old contents are already reloaded.
                let size = target.spill_slot_size(dst.0, abi);
                if slots.get(slot).is_none_or(|&(_, held, _)| held < size) {
                    let fresh = (
                        frame.alloc(size, abi.stack.slot_size),
                        size,
                        SlotChain::default(),
                    );
                    match slots.get_mut(slot) {
                        Some(known) => *known = fresh,
                        None => slots.push(fresh),
                    }
                }
                let (offset, _, chain) = &mut slots[slot];
                let value = pinned(context, assignment, dst);
                let store = target.emit_spill_store(context, value, dst.0, &abi.sp, *offset);
                chain.change(context, before, store.as_ref())?;
                context.insert_op_before(before, store.as_ref())?;
            }
            Step::Reload(slot) => {
                let (offset, _, chain) = &mut slots[slot];
                let value = pinned(context, assignment, dst);
                let reload = target.emit_spill_reload(context, value, dst.0, &abi.sp, *offset);
                chain.observe(context, before, reload.as_ref())?;
                context.insert_op_before(before, reload.as_ref())?;
            }
        }
    }
    Ok(())
}

// A fresh value of `register`'s class, assigned to it: what a spill hook
// names a physical register through.
fn pinned(context: &Context, assignment: &mut HashMap<u32, PhysReg>, register: PhysReg) -> ValueId {
    let value = fresh_reg(context, register.0);
    assignment.insert(value.number(), register);
    value
}

// The registers named directly by `op` or an op after it in its block before
// anything writes them: what a fixed-register window holds across `op`.
fn named_live(context: &Context, op: OpId, assignment: &HashMap<u32, PhysReg>) -> Vec<PhysReg> {
    let Some(block) = context.parent_block(op) else {
        return Vec::new();
    };
    let ops = context.get_block(block).op_ids();
    let from = ops.iter().position(|&id| id == op).unwrap_or(ops.len());
    let overlaps = |a: &PhysReg, b: &PhysReg| a.0.overlaps(a.1, b.0, b.1);
    let mut live: Vec<PhysReg> = Vec::new();
    let mut written: Vec<PhysReg> = Vec::new();
    for &id in &ops[from..] {
        let regs = op_regs(&context.get_op(id));
        for register in regs.phys_uses {
            if !written.iter().any(|w| overlaps(w, &register)) && !live.contains(&register) {
                live.push(register);
            }
        }
        written.extend(regs.phys_defs);
        let defs = regs
            .defs
            .iter()
            .filter_map(|def| assignment.get(&def.number()));
        written.extend(defs);
    }
    live
}

// The registers a cycle of `class` may be broken through. A callee-saved
// register is saved for the caller only when some value took it.
fn scratch(abi: &AbiInfo, assignment: &HashMap<u32, PhysReg>, class: RegClassId) -> Vec<PhysReg> {
    let overlaps = |a: &PhysReg, b: &PhysReg| a.0.overlaps(a.1, b.0, b.1);
    let unsaved = |register: &PhysReg| {
        abi.callee_saved.iter().any(|saved| {
            overlaps(saved, register) && !assignment.values().any(|taken| overlaps(taken, saved))
        })
    };
    allocation_order(abi, class)
        .into_iter()
        .filter(|register| !unsaved(register))
        .collect()
}

enum Step {
    Move(PhysReg),
    Replay(OpId),
    Store(usize),
    Reload(usize),
}

// Sequence a parallel copy of `dst <- src` moves, destinations unique: emit
// a move whose destination no other move still reads; when only cycles are
// left, move a register the first destination overlaps aside, into a
// `scratch` register no value live into the successor and no move holds, or
// into a stack slot when there is none, and read it from there.
fn sequence(
    mut moves: Vec<(PhysReg, Source)>,
    live: &[PhysReg],
    scratch: impl Fn(RegClassId) -> Vec<PhysReg>,
) -> Vec<(PhysReg, Step)> {
    let overlaps = |a: PhysReg, b: PhysReg| a.0.overlaps(a.1, b.0, b.1);
    let reads = |moves: &[(PhysReg, Source)], dst: PhysReg| {
        moves
            .iter()
            .any(|&(_, src)| matches!(src, Source::Reg(src) if overlaps(src, dst)))
    };
    let mut steps = Vec::new();
    while !moves.is_empty() {
        if let Some(at) = moves.iter().position(|&(dst, _)| !reads(&moves, dst)) {
            let (dst, src) = moves.remove(at);
            steps.push(match src {
                Source::Reg(src) => (dst, Step::Move(src)),
                Source::Slot(slot) => (dst, Step::Reload(slot)),
                Source::Const(def) => (dst, Step::Replay(def)),
            });
            continue;
        }
        // Only cycles are left: free the first destination, one register
        // reading it at a time, the same register first.
        let (blocked, _) = moves[0];
        let reading = |exact: bool| {
            moves.iter().find_map(|&(_, src)| match src {
                Source::Reg(src) if src == blocked || !exact && overlaps(src, blocked) => Some(src),
                _ => None,
            })
        };
        let aside = reading(true)
            .or_else(|| reading(false))
            .expect("a destination left is read");
        let busy = |register: PhysReg| {
            live.iter().any(|&other| overlaps(other, register))
                || moves.iter().any(|&(dst, src)| {
                    overlaps(dst, register)
                        || matches!(src, Source::Reg(src) if overlaps(src, register))
                })
        };
        let free = scratch(aside.0)
            .into_iter()
            .find(|&register| !busy(register));
        let to = match free {
            Some(free) => {
                steps.push((free, Step::Move(aside)));
                Source::Reg(free)
            }
            None => {
                let pending = |slot| moves.iter().any(|&(_, src)| src == Source::Slot(slot));
                let slot = (0..).find(|&slot| !pending(slot)).expect("a free slot");
                steps.push((aside, Step::Store(slot)));
                Source::Slot(slot)
            }
        };
        for (_, src) in moves.iter_mut() {
            if *src == Source::Reg(aside) {
                *src = to;
            }
        }
    }
    steps
}
