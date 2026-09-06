//! Full unroll of a short counted loop.
//!
//! A loop whose trip count is a small constant and whose body is small becomes
//! that many copies of the body, each with the counter spelled as the literal it
//! takes. Nothing is decided here: what makes the copies worth their size is
//! that the address arithmetic in them folds, which `instcombine` does
//! afterwards.

use std::collections::HashMap;

use crate::analysis::affine::{AffineView, Loop, body_block, body_ops, carried, nests_under};
use crate::{Context, OpId, OperationRef, PassError, Rewriter, Theta, ValueId};

use super::lower::{erase_unread, literal};

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
    let ops = body_ops(context, level.op)?;
    let handle = context.get_op(level.op);
    // An ordered body that opens a token scope leaves the loop through a
    // `break`, which is not an iteration a copy can stand for.
    if let Some(block) = body_block(context, level.op)
        && context.get_block(block).arguments().len() != carried(context, &handle)?.args.len()
    {
        return None;
    }
    ((1..=UNROLL_TRIP).contains(&trip)
        && level.lower.as_constant().is_some()
        && level.step.as_constant().is_some()
        && ops.len() <= UNROLL_BUDGET)
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
    let target = OperationRef::new(handle.clone());
    let region = *handle.regions().last().expect("a loop has a body");
    let (arguments, mut incoming) = match handle.clone().as_interface::<dyn Theta>() {
        Some(theta) => {
            let ports = carried(context, &handle).expect("a loop carries ports");
            let body = context.get_region(theta.body());
            let mut arguments = ports.args;
            arguments.extend(body.dep_arguments().iter().map(crate::Value::id));
            let mut inits = ports.inits;
            inits.extend(handle.dep_operands());
            (arguments, inits)
        }
        None => {
            let carried = handle
                .clone()
                .as_interface::<dyn crate::LoopLike>()
                .expect("a counted loop carries ports");
            (carried.carried_args(), carried.inits())
        }
    };
    let parent = context.parent_nodes_region(level.op);

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
            let value = match parent {
                Some(region) => {
                    let mut site = super::lower::Site::Region(region);
                    super::lower::literal_at(context, &mut site, lower + iteration * step, ty)
                }
                None => literal(context, rewriter, &target, lower + iteration * step, ty)?,
            };
            bindings.insert(counter, value);
        }
        incoming = match parent {
            Some(region) => copy_body_nodes(context, rewriter, region, &bindings, &target)?,
            None => copy_body(context, rewriter, region, &bindings, &target)?,
        };
    }

    for (&result, &value) in handle.results().iter().zip(&incoming) {
        context.replace_value_uses(result, value);
        if let Some(region) = parent {
            context.rename_region_results(region, result, value, &[]);
        }
    }
    rewriter.erase_op(&target)
}

/// [`copy_body`] for an unordered body: the copy joins the loop's own region,
/// and the values its next iteration would take are what the copy hands on.
fn copy_body_nodes(
    context: &Context,
    rewriter: &mut Rewriter,
    destination: crate::RegionId,
    bindings: &HashMap<ValueId, ValueId>,
    target: &OperationRef,
) -> Result<Vec<ValueId>, PassError> {
    let theta = target
        .op()
        .clone()
        .as_interface::<dyn Theta>()
        .expect("an unordered loop declares a theta");
    let body = theta.body();
    let binding = theta.carried();
    let (ops, results) = crate::clone::clone_nodes_ops_into(context, body, bindings, destination);
    let values = context.get_region(body).value_results().len();
    let deps = (results.len() - values) / 2;
    let mut leaving = results[binding.continue_.clone()].to_vec();
    leaving.extend(results[values..values + deps].iter().copied());
    erase_unread(context, rewriter, &ops)?;
    Ok(leaving)
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
