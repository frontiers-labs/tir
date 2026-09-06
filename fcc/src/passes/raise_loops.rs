//! Loop raising.
//!
//! The `cir` loop ops carry the shape C wrote: which region tests, which steps,
//! which is the body. This pass reads that shape and, where the loop provably
//! counts, emits `scf.for` — the mid-end's counted loop, and the input the affine
//! view is defined on. Every loop it refuses becomes the header, body, step and
//! exit blocks a frontend would have emitted directly, which `restructure` raises
//! as it always has. Refusal is the default outcome: a loop the analysis cannot
//! prove counted is a missed optimisation, never a miscompilation.

use tir::analysis::exits::resolve_exit_target;
use tir::analysis::{AnalysisManager, Escape, EscapeFacts};
use tir::attributes::AttributeValue;
use tir::builtin::{AddIOp, CmpIOp, ConstantOp, ops as b};
use tir::cfg::ops as cb;
use tir::func::FuncOp;
use tir::ptr::{AllocaOp, LoadOp, StoreOp, ops as p};
use tir::scf;
use tir::{
    BlockHandle, BlockId, Context, MemoryRead, MemoryWrite, OpHandle, OpId, Operation,
    OperationRef, Pass, PassError, PassTarget, RegionId, RegionKind, Rewriter, TypeId, ValueId,
};

use crate::cir;

#[derive(Default)]
pub struct RaiseLoopsPass;

impl RaiseLoopsPass {
    pub fn new() -> Self {
        Self
    }
}

tir::register_pass!(RaiseLoopsPass, "raise-loops");

impl Pass for RaiseLoopsPass {
    fn name(&self) -> &'static str {
        "raise-loops"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Blocks)
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        // Innermost first, so a nest collapses from the bottom up: by the time a
        // loop is looked at, every loop it holds is already an `scf.for` op or a
        // graph of blocks.
        for loop_op in loops_in(context, body) {
            let shape = shape_of(context, loop_op).expect("only loop ops are collected");
            // Read per loop: raising one edits the function, which is exactly when
            // the manager rebuilds.
            let escapes = analyses.get::<EscapeFacts>(context, op.op().id);
            match recognise(context, &escapes, loop_op, &shape) {
                Some(counted) => raise(context, rewriter, loop_op, &shape, counted)?,
                None => flatten(context, rewriter, loop_op)?,
            }
        }
        Ok(())
    }
}

/// Every `cir` loop op under `region`, innermost first.
fn loops_in(context: &Context, region: RegionId) -> Vec<OpId> {
    let mut found = Vec::new();
    for block in context.get_region(region).block_ids() {
        for op_id in context.get_block(block).op_ids() {
            for nested in context.get_op(op_id).regions().to_vec() {
                found.extend(loops_in(context, nested));
            }
            if shape_of(context, op_id).is_some() {
                found.push(op_id);
            }
        }
    }
    found
}

/// The regions of a `cir` loop, whichever of the three spellings it has.
struct Shape {
    condition: RegionId,
    /// A `for`'s step clause, which runs between the body and the condition.
    step: Option<RegionId>,
    body: RegionId,
    /// Whether the condition is tested before the first iteration.
    head_controlled: bool,
}

fn shape_of(context: &Context, op_id: OpId) -> Option<Shape> {
    let op = context.get_op(op_id);
    let regions = op.regions().to_vec();
    if op.is::<cir::ForOp>() {
        Some(Shape {
            condition: regions[0],
            step: Some(regions[1]),
            body: regions[2],
            head_controlled: true,
        })
    } else if op.is::<cir::WhileOp>() {
        Some(Shape {
            condition: regions[0],
            step: None,
            body: regions[1],
            head_controlled: true,
        })
    } else if op.is::<cir::DoOp>() {
        Some(Shape {
            body: regions[0],
            condition: regions[1],
            step: None,
            head_controlled: false,
        })
    } else {
        None
    }
}

/// Spill a loop's regions into the region holding it, turning each terminator
/// that used to leave a region into the branch it stood for.
fn flatten(context: &Context, rewriter: &mut Rewriter, op_id: OpId) -> Result<(), PassError> {
    let shape = shape_of(context, op_id).expect("only loop ops are flattened");
    let block = context
        .parent_block(op_id)
        .expect("a loop op sits in a block");
    let region = context
        .parent_region(block)
        .expect("a block sits in a region");

    let condition = entry_of(context, shape.condition);
    let body = entry_of(context, shape.body);
    // What the body falls through to: a `for` steps first, everything else tests
    // again straight away.
    let latch = shape.step.map_or(condition, |step| entry_of(context, step));

    // Everything after the loop is what its exit reaches.
    let position = context
        .get_block(block)
        .op_ids()
        .iter()
        .position(|&other| other == op_id)
        .expect("the loop op sits in the block holding it");
    let exit = rewriter.split_block(block, position + 1).id();
    context.get_region(region).add_block(exit);

    spill(
        context,
        rewriter,
        op_id,
        shape.condition,
        region,
        body,
        exit,
    )?;
    spill(context, rewriter, op_id, shape.body, region, latch, exit)?;
    if let Some(step) = shape.step {
        spill(context, rewriter, op_id, step, region, condition, exit)?;
    }

    let head = if shape.head_controlled {
        condition
    } else {
        body
    };
    let block = context.get_block(block);
    rewriter.erase_op(&OperationRef::new(context.get_op(op_id)))?;
    block.append_op(cb::br(context, vec![], head).build());
    Ok(())
}

/// Replace the terminators that leave `source` with the branches they stand for —
/// a `cir.condition` picks between `enter` and `exit`, a `cir.break` leaving this
/// loop goes to `exit`, and a fallthrough or a `cir.continue` of this loop goes
/// on to `enter` — then move its blocks into `destination`. An exit naming an
/// outer loop stays for that loop's turn; blocks leaving through an ordinary
/// branch are already right.
fn spill(
    context: &Context,
    rewriter: &mut Rewriter,
    loop_op: OpId,
    source: RegionId,
    destination: RegionId,
    enter: BlockId,
    exit: BlockId,
) -> Result<(), PassError> {
    for block in context.get_region(source).block_ids() {
        let block = context.get_block(block);
        let last = *block
            .op_ids()
            .last()
            .expect("every block ends in a terminator");
        let op = context.get_op(last);
        let target = OperationRef::new(op.clone());
        let leaves_this_loop = || resolve_exit_target(context, last).ok() == Some(loop_op);
        if op.is::<cir::ConditionOp>() {
            let branch =
                cb::cond_br(context, op.operands()[0], vec![], vec![], enter, exit).build();
            rewriter.replace_op(&target, &branch)?;
        } else if op.is::<cir::BreakOp>() && leaves_this_loop() {
            let branch = cb::br(context, vec![], exit).build();
            rewriter.replace_op(&target, &branch)?;
        } else if op.is::<cir::YieldOp>() || (op.is::<cir::ContinueOp>() && leaves_this_loop()) {
            let branch = cb::br(context, vec![], enter).build();
            rewriter.replace_op(&target, &branch)?;
        }
    }
    rewriter.splice_region(source, destination);
    Ok(())
}

/// The entry block of a loop region: where control arrives when the region runs.
fn entry_of(context: &Context, region: RegionId) -> BlockId {
    context.get_region(region).block_ids()[0]
}

/// A `cir.for` recognised as counting: what it counts through, where its counter
/// lives, and the bounds and step `scf.for` needs.
struct Counted {
    /// The stack slot the counter lives in, an `ptr.alloca` private to the
    /// function.
    slot: ValueId,
    counter: TypeId,
    upper: Bound,
    /// The constant the step adds, positive.
    step: i64,
    body: BlockId,
}

/// How the upper bound is read where the loop op sits, given that the condition
/// region reads it where the loop op is not.
enum Bound {
    /// Already available: defined outside the loop, so nothing to repeat.
    Available(ValueId),
    /// A constant, spelled again.
    Constant(i64),
    /// A load of a slot the loop never writes, repeated.
    Load(ValueId),
}

/// Read a `cir.for` as a counted loop, or refuse it.
///
/// What has to hold is what `scf.for` means: a counter private to the loop,
/// stepping by a constant, tested against a bound the loop cannot change. Every
/// condition below is one of those three, and anything the analysis cannot see
/// through is refused — the loop still runs, as blocks and branches.
fn recognise(
    context: &Context,
    escapes: &EscapeFacts,
    op_id: OpId,
    shape: &Shape,
) -> Option<Counted> {
    let condition = only_block(context, shape.condition)?;
    let step = only_block(context, shape.step?)?;
    let body = only_block(context, shape.body)?;

    let (slot, upper, tested) = counted_test(context, &context.get_block(condition))?;
    let stepped = counted_step(context, &context.get_block(step), slot)?;
    // `scf.for` counts through one type: what the step produces is what the latch
    // compares, so the test must already read the counter at that width.
    let counter = context.get_value(stepped.increment).ty();
    if tested != counter {
        return None;
    }
    // A body leaving through `break` or `continue` is not a plain iteration.
    if !context
        .get_block(body)
        .op_ids()
        .last()
        .is_some_and(|&last| context.get_op(last).is::<cir::YieldOp>())
    {
        return None;
    }

    // The counter is the loop's own: nothing outside holds its address, and
    // inside the loop the only writer is the step.
    if escapes.escape(slot) != Escape::Local {
        return None;
    }
    if !reached_only_by_reading(context, op_id, slot, &[stepped.store]) {
        return None;
    }

    // The bound must mean the same on every iteration.
    let upper = match upper {
        Bound::Load(bound) if !invariant_slot(context, escapes, op_id, bound) => return None,
        upper => upper,
    };

    Some(Counted {
        slot,
        counter,
        upper,
        step: stepped.step,
        body,
    })
}

/// The one block of a single-block region, or `None` for a region holding a CFG.
fn only_block(context: &Context, region: RegionId) -> Option<BlockId> {
    match context.get_region(region).block_ids()[..] {
        [block] => Some(block),
        _ => None,
    }
}

/// Read `%a = ptr.load S; %b = ...; %p = cmpi %a, %b {slt}; cir.condition %p`,
/// the only test v1 counts through. The block must hold nothing else: what it
/// holds runs once per iteration, and `scf.for`'s latch would not run it.
fn counted_test(context: &Context, block: &BlockHandle) -> Option<(ValueId, Bound, TypeId)> {
    let ops = block.op_ids();
    let (&last, rest) = ops.split_last()?;
    let terminator = context.get_op(last);
    if !terminator.is::<cir::ConditionOp>() {
        return None;
    }
    let compare = definer(context, rest, terminator.operands()[0])?;
    if !compare.is::<CmpIOp>()
        || compare.attr("predicate")
            != Some(AttributeValue::Predicate(tir::attributes::Predicate::Slt))
    {
        return None;
    }
    let load = definer(context, rest, compare.operands()[0])?;
    let slot = load
        .clone()
        .as_interface::<dyn MemoryRead>()
        .filter(|read| read.read_location() == load.operands()[0])
        .map(|_| load.operands()[0])?;
    if !definition(context, slot).is_some_and(|def| def.is::<AllocaOp>()) {
        return None;
    }

    let bound = compare.operands()[1];
    let (upper, spelled) = match definer(context, rest, bound) {
        // Nothing in this block defines it, so it is defined before the loop.
        None => (Bound::Available(bound), 0),
        Some(def) if def.is::<ConstantOp>() => (Bound::Constant(int_attr(&def, "value")?), 1),
        Some(def) if def.is::<LoadOp>() => (Bound::Load(def.operands()[0]), 1),
        Some(_) => return None,
    };
    // The terminator, the compare, the counter's load, and whatever spells the
    // bound: nothing else may run per iteration.
    let tested = context.get_value(bound).ty();
    (ops.len() == 3 + spelled).then_some((slot, upper, tested))
}

/// What a counted loop's step region does: read the counter, add a constant, and
/// write it back, and nothing else.
struct Step {
    store: OpId,
    increment: ValueId,
    step: i64,
}

fn counted_step(context: &Context, block: &BlockHandle, slot: ValueId) -> Option<Step> {
    let ops = block.op_ids();
    let (&last, rest) = ops.split_last()?;
    if !context.get_op(last).is::<cir::YieldOp>() || rest.len() != 4 {
        return None;
    }
    let store = rest
        .iter()
        .map(|&op| context.get_op(op))
        .find(|op| op.is::<StoreOp>())?;
    let write = store.clone().as_interface::<dyn MemoryWrite>()?;
    if write.write_location() != slot {
        return None;
    }
    let increment = write.written_value();
    let add = definer(context, rest, increment)?;
    if !add.is::<AddIOp>() {
        return None;
    }
    // `i + 1` and `1 + i` count the same.
    let step = [(0, 1), (1, 0)].into_iter().find_map(|(read, spelled)| {
        let read = definer(context, rest, add.operands()[read])?;
        let spelled = definer(context, rest, add.operands()[spelled])?;
        if !read.is::<LoadOp>() || read.operands()[0] != slot || !spelled.is::<ConstantOp>() {
            return None;
        }
        int_attr(&spelled, "value")
    })?;
    // A step of zero never ends and a negative one counts the other way; `scf.for`
    // tests `counter < bound`, which only a positive step reaches.
    (step > 0).then_some(Step {
        store: store.id,
        increment,
        step,
    })
}

/// Whether the only reach the loop has into `slot` is reading it, plus `writes`.
///
/// The rule is on the *uses of the address*, not on the write locations: escape
/// facts follow `ptradd`, so a store through a pointer derived from the slot
/// leaves the slot `Local` while still changing what it holds. Anything naming
/// the address that is not a direct read, and not one of `writes`, refuses.
fn reached_only_by_reading(context: &Context, op_id: OpId, slot: ValueId, writes: &[OpId]) -> bool {
    ops_under(context, op_id).into_iter().all(|other| {
        let op = context.get_op(other);
        if !op.operands().contains(&slot) {
            return true;
        }
        writes.contains(&other)
            || op
                .clone()
                .as_interface::<dyn MemoryRead>()
                .is_some_and(|read| read.read_location() == slot)
    })
}

/// Whether `slot` reads the same on every iteration: the loop only reads it, and
/// no call inside can write it, because nothing outside holds its address.
fn invariant_slot(context: &Context, escapes: &EscapeFacts, op_id: OpId, slot: ValueId) -> bool {
    definition(context, slot).is_some_and(|def| def.is::<AllocaOp>())
        && escapes.escape(slot) == Escape::Local
        && reached_only_by_reading(context, op_id, slot, &[])
}

/// Every operation under `op`, at any depth.
fn ops_under(context: &Context, op: OpId) -> Vec<OpId> {
    let mut found = Vec::new();
    for region in context.get_op(op).regions().to_vec() {
        for block in context.get_region(region).block_ids() {
            for op_id in context.get_block(block).op_ids() {
                found.push(op_id);
                found.extend(ops_under(context, op_id));
            }
        }
    }
    found
}

/// The operation among `ops` that defines `value`.
fn definer(context: &Context, ops: &[OpId], value: ValueId) -> Option<OpHandle> {
    ops.iter()
        .map(|&op| context.get_op(op))
        .find(|op| op.results().contains(&value))
}

/// The operation defining `value`, wherever it sits.
fn definition(context: &Context, value: ValueId) -> Option<OpHandle> {
    context
        .get_value(value)
        .defining_op()
        .map(|op| context.get_op(op))
}

fn int_attr(op: &OpHandle, name: &str) -> Option<i64> {
    match op.attr(name)? {
        AttributeValue::Int(value) => Some(value),
        _ => None,
    }
}

/// Emit the `scf.for` a recognised loop stands for.
///
/// The counter leaves its slot for a carried port, but the slot stays: the body
/// still loads it, so the port is written back at the top of every iteration and
/// the value the loop ends on is written back after it. That traffic is what
/// promotion, folding and state erasure exist to remove — the same division of
/// labour `restructure` already relies on.
fn raise(
    context: &Context,
    rewriter: &mut Rewriter,
    op_id: OpId,
    shape: &Shape,
    counted: Counted,
) -> Result<(), PassError> {
    let target = OperationRef::new(context.get_op(op_id));

    let lower = p::load(context, counted.slot, counted.counter).build();
    rewriter.insert_op_before(&target, &lower)?;
    let upper = match counted.upper {
        Bound::Available(value) => value,
        Bound::Constant(value) => {
            let constant = b::constant(context, value, counted.counter).build();
            rewriter.insert_op_before(&target, &constant)?;
            constant.result()
        }
        Bound::Load(slot) => {
            let load = p::load(context, slot, counted.counter).build();
            rewriter.insert_op_before(&target, &load)?;
            load.result()
        }
    };
    let step = b::constant(context, counted.step, counted.counter).build();
    rewriter.insert_op_before(&target, &step)?;

    // The body reads the counter where it always did — through the slot — so the
    // carried port is written there before anything else runs.
    let iteration = context
        .append_block_argument(counted.body, counted.counter)
        .id();
    let body = context.get_block(counted.body);
    let write_back = p::store(context, iteration, counted.slot).build();
    body.insert(0, write_back.id());

    let advance = b::addi(context, iteration, step.result(), counted.counter).build();
    let latch = scf::r#yield(context, vec![advance.result()]).build();
    let terminator = *body.op_ids().last().expect("the body ends in cir.yield");
    body.insert(body.op_ids().len() - 1, advance.id());
    rewriter.replace_op(&OperationRef::new(context.get_op(terminator)), &latch)?;

    let region = context.create_region();
    rewriter.splice_region(shape.body, region.id());
    let raised = scf::ForLegacyOpBuilder::new(context)
        .lower_bound(lower.result())
        .upper_bound(upper)
        .step(step.result())
        .inits(vec![lower.result()])
        .result_types(vec![counted.counter])
        .body(region.id())
        .build();
    rewriter.insert_op_before(&target, &raised)?;

    let counted_out = context.get_op(raised.id()).results()[0];
    let final_value = p::store(context, counted_out, counted.slot).build();
    rewriter.insert_op_before(&target, &final_value)?;
    rewriter.erase_op(&target)
}
