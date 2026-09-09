//! Full unroll of a short counted loop.
//!
//! A loop whose trip count is a small constant and whose body is small becomes
//! that many copies of the body, each with the counter spelled as the literal it
//! takes. Nothing is decided here: what makes the copies worth their size is
//! that the address arithmetic in them folds, which `instcombine` does
//! afterwards.

use std::collections::HashMap;

use crate::analysis::affine::{AffineView, Loop, body_ops, nests_under};
use crate::{Context, OpId, OperationRef, PassError, Theta, ValueId};

use super::lower::erase_unread;

/// The most iterations a loop is unrolled whole. A knob.
pub const UNROLL_TRIP: i128 = 8;

/// The most operations a body may hold and still be copied that many times. A
/// knob.
pub const UNROLL_BUDGET: usize = 32;

pub(super) fn run(context: &Context, root: OpId) -> Result<(), PassError> {
    for view in nests_under(context, root) {
        if let Some(level) = worth_unrolling(context, &view) {
            unroll(context, level)?;
        }
    }
    Ok(())
}

/// The innermost loop of a nest, where it is short and small enough to copy.
fn worth_unrolling<'a>(context: &Context, view: &'a AffineView) -> Option<&'a Loop> {
    let level = view.loops.last()?;
    let trip = level.trip?;
    let ops = body_ops(context, level.op)?;
    // The copies join the region the loop stands in, which an unordered region
    // alone has.
    context.parent_nodes_region(level.op)?;
    ((1..=UNROLL_TRIP).contains(&trip)
        && level.lower.as_constant().is_some()
        && level.step.as_constant().is_some()
        && ops.len() <= UNROLL_BUDGET)
        .then_some(level)
}

/// Replace the loop with one copy of its body per iteration, threading the
/// carried ports from each copy into the next.
fn unroll(context: &Context, level: &Loop) -> Result<(), PassError> {
    let (lower, step, trip) = (
        level.lower.as_constant().expect("a constant lower bound"),
        level.step.as_constant().expect("a constant step"),
        level.trip.expect("a constant trip count"),
    );
    let handle = context.get_op(level.op);
    let target = OperationRef::new(handle.clone());
    let sides = crate::binding::carried(context, &handle);
    let arguments: Vec<ValueId> = sides.iter().map(|side| side.port).collect();
    let mut incoming: Vec<ValueId> = sides.iter().map(|side| side.init).collect();
    let parent = context
        .parent_nodes_region(level.op)
        .expect("an unrolled loop stands in an unordered region");
    let mut copies = Vec::new();

    for iteration in 0..trip {
        let mut bindings: HashMap<ValueId, ValueId> = arguments
            .iter()
            .zip(&incoming)
            .map(|(&argument, &value)| (argument, value))
            .collect();
        // The counter is the port the loop steps; every copy names it outright.
        let counting = level.counter.iter().chain(&level.counter_aliases);
        for &counter in counting {
            let ty = context.get_value(counter).ty();
            let value = super::lower::literal_at(context, parent, lower + iteration * step, ty);
            bindings.insert(counter, value);
        }
        let (ops, leaving) = copy_body_nodes(context, parent, &bindings, &target);
        copies.extend(ops);
        incoming = leaving;
    }

    for (&result, &value) in handle.results().iter().zip(&incoming) {
        context.replace_value_uses(result, value);
        context.rename_region_results(parent, result, value, &[]);
    }
    context.erase_op(&target)?;
    // Only now is it known which copies nothing reads: the last copy's latch
    // is what the loop's results became.
    erase_unread(context, &copies)
}

/// One copy of the body, joining the region the loop stands in; the values its
/// next iteration would take are what the copy hands on.
fn copy_body_nodes(
    context: &Context,
    destination: crate::RegionId,
    bindings: &HashMap<ValueId, ValueId>,
    target: &OperationRef,
) -> (Vec<OpId>, Vec<ValueId>) {
    let theta = target
        .op()
        .clone()
        .as_interface::<dyn Theta>()
        .expect("an unordered loop declares a theta");
    let body = theta.body();
    let binding = theta.binding();
    let (ops, results) = crate::clone::clone_nodes_ops_into(context, body, bindings, destination);
    (ops, results[binding.continue_].to_vec())
}
