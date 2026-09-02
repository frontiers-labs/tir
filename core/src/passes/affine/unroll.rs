//! Full unroll of a short counted loop.
//!
//! A loop whose trip count is a small constant and whose body is small becomes
//! that many copies of the body, each with the counter spelled as the literal it
//! takes. Nothing is decided here: what makes the copies worth their size is
//! that the address arithmetic in them folds, which `instcombine` does
//! afterwards.

use std::collections::HashMap;

use crate::analysis::affine::{AffineView, Loop, body_block, nests_under};
use crate::{Context, LoopLike, OpId, OperationRef, PassError, Rewriter, ValueId};

use super::lower::literal;

/// The most iterations a loop is unrolled whole. A knob.
pub const UNROLL_TRIP: i128 = 8;

/// The most operations a body may hold and still be copied that many times. A
/// knob.
pub const UNROLL_BUDGET: usize = 32;

pub(super) fn run(context: &Context, rewriter: &mut Rewriter, root: OpId) -> Result<(), PassError> {
    for view in nests_under(context, root) {
        if let Some(level) = worth_unrolling(context, &view) {
            unroll(context, rewriter, level)?;
        }
    }
    Ok(())
}

/// The innermost loop of a nest, where it is short and small enough to copy.
fn worth_unrolling<'a>(context: &Context, view: &'a AffineView) -> Option<&'a Loop> {
    let level = view.loops.last()?;
    let trip = level.trip?;
    let block = body_block(context, level.op)?;
    let carried = context.get_op(level.op).as_interface::<dyn LoopLike>()?;
    // A body that opens a token scope leaves the loop through a `break`, which
    // is not an iteration a copy can stand for.
    ((1..=UNROLL_TRIP).contains(&trip)
        && context.get_block(block).arguments().len() == carried.carried_args().len()
        && level.lower.as_constant().is_some()
        && level.step.as_constant().is_some()
        && context.get_block(block).op_ids().len() <= UNROLL_BUDGET)
        .then_some(level)
}

/// Replace the loop with one copy of its body per iteration, threading the
/// carried ports from each copy into the next.
fn unroll(context: &Context, rewriter: &mut Rewriter, level: &Loop) -> Result<(), PassError> {
    let (lower, step, trip) = (
        level.lower.as_constant().expect("a constant lower bound"),
        level.step.as_constant().expect("a constant step"),
        level.trip.expect("a constant trip count"),
    );
    let handle = context.get_op(level.op);
    let carried = handle
        .clone()
        .as_interface::<dyn LoopLike>()
        .expect("a counted loop carries ports");
    let target = OperationRef::new(handle.clone());
    let region = *handle.regions().last().expect("a loop has a body");
    let arguments = carried.carried_args();

    let mut incoming = carried.inits();
    for iteration in 0..trip {
        let mut bindings: HashMap<ValueId, ValueId> = arguments
            .iter()
            .zip(&incoming)
            .map(|(&argument, &value)| (argument, value))
            .collect();
        // The counter is the port the loop steps; every copy names it outright.
        if let Some(counter) = level.counter {
            let ty = context.get_value(counter).ty();
            let value = literal(context, rewriter, &target, lower + iteration * step, ty)?;
            bindings.insert(counter, value);
        }
        incoming = copy_body(context, rewriter, region, &bindings, &target)?;
    }

    for (&result, &value) in handle.results().iter().zip(&incoming) {
        context.replace_value_uses(result, value);
    }
    rewriter.erase_op(&target)
}

/// One copy of the body, spelled where the loop stood, and the values its
/// terminator hands the next copy.
fn copy_body(
    context: &Context,
    rewriter: &mut Rewriter,
    region: crate::RegionId,
    bindings: &HashMap<ValueId, ValueId>,
    target: &OperationRef,
) -> Result<Vec<ValueId>, PassError> {
    let copy = crate::clone_region_with_mapping(context, region, bindings);
    let block = context.get_block(context.get_region(copy).block_ids()[0]);
    let last = *block.op_ids().last().expect("a body is terminated");
    let leaving = context.get_op(last).operands().to_vec();
    rewriter.erase_op(&OperationRef::new(context.get_op(last)))?;
    let destination = target
        .op()
        .parent_block()
        .expect("the loop sits in a block");
    let position = context
        .get_block(destination)
        .op_ids()
        .iter()
        .position(|&other| other == target.op().id)
        .expect("the loop sits in the block holding it");
    for (offset, op) in block.op_ids().into_iter().enumerate() {
        block.remove_op(op);
        context.get_block(destination).insert(position + offset, op);
    }
    rewriter.erase_block(block.id());
    Ok(leaving)
}
