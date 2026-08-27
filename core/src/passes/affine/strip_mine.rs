//! Strip-mining one counted loop: the whole tiles first, then what is left.
//!
//! `for i = lb; i < ub; i += s { body }` becomes
//!
//! ```text
//! last = lb + ((ub - lb) / (s·t)) · (s·t)
//! for it = lb; it < last; it += s·t {
//!   for i = it; i < it + s·t; i += s { body }
//! }
//! for i = final; i < ub; i += s { body }
//! ```
//!
//! The span is divided unsigned: a loop may run more iterations than its
//! counter's signed width holds, and `ub - lb` is exact modulo that width. The
//! remainder is entered on the value the tile loop's counter ended at rather
//! than on `last`: where the loop runs no iteration at all, `last` lies below
//! `lb`, and a remainder entered there would count what the loop never did.

use crate::analysis::affine::counter_port;
use crate::builtin::ops as b;
use crate::{
    Context, CountedLoop, LoopLike, OpId, Operation, OperationRef, PassError, Rewriter, TypeId,
    Value, ValueId, scf,
};

/// Replace `op` with its tiled form: the loop over whole tiles of `tile`
/// iterations, then the loop over the remainder, in that order.
pub fn strip_mine(
    context: &Context,
    rewriter: &mut Rewriter,
    op: OpId,
    tile: i128,
) -> Result<(OpId, OpId), PassError> {
    let handle = context.get_op(op);
    let (Some(counted), Some(carried), Some(counter)) = (
        handle.clone().as_interface::<dyn CountedLoop>(),
        handle.clone().as_interface::<dyn LoopLike>(),
        counter_port(context, &handle),
    ) else {
        return Err(PassError::RewriteFailed(op));
    };
    let port = carried
        .carried_args()
        .iter()
        .position(|&argument| argument == counter)
        .expect("the counter is carried");
    let body = *handle
        .regions()
        .last()
        .ok_or(PassError::RewriteFailed(op))?;
    let types: Vec<TypeId> = handle
        .results()
        .iter()
        .map(|&result| context.get_value(result).ty())
        .collect();
    // A body opening a token scope leaves the loop through a `break`, which a
    // copy cannot stand for.
    if context
        .get_block(context.get_region(body).block_ids()[0])
        .arguments()
        .len()
        != types.len()
    {
        return Err(PassError::RewriteFailed(op));
    }
    let block = context
        .parent_block(op)
        .ok_or(PassError::MissingBlock("scf.for"))?;
    let target = OperationRef::new(handle.clone(), Some(context.get_block(block)), None);
    let (lower, upper, step) = (counted.lower_bound(), counted.upper_bound(), counted.step());
    let ty = context.get_value(lower).ty();

    let tile = b::constant(context, tile as i64, ty).build();
    rewriter.insert_op_before(&target, &tile)?;
    let stride = b::muli(context, step, tile.result(), ty).build();
    rewriter.insert_op_before(&target, &stride)?;
    let span = b::subi(context, upper, lower, ty).build();
    rewriter.insert_op_before(&target, &span)?;
    let tiles = b::divui(context, span.result(), stride.result(), ty).build();
    rewriter.insert_op_before(&target, &tiles)?;
    let covered = b::muli(context, tiles.result(), stride.result(), ty).build();
    rewriter.insert_op_before(&target, &covered)?;
    let last = b::addi(context, lower, covered.result(), ty).build();
    rewriter.insert_op_before(&target, &last)?;

    let tile_region = context.create_region();
    let tile_args: Vec<Value> = types
        .iter()
        .map(|&ty| context.create_value(ty, None))
        .collect();
    let tile_block = context.create_block(tile_args.clone());
    tile_region.add_block(tile_block.id());
    let base = tile_args[port].id();
    let end = tile_block.append_op(b::addi(context, base, stride.result(), ty).build());
    let inner = tile_block.append_op(
        scf::ForOpBuilder::new(context)
            .lower_bound(base)
            .upper_bound(end.result())
            .step(step)
            .inits(tile_args.iter().map(Value::id).collect())
            .result_types(types.clone())
            .body(crate::clone::clone_region(context, body))
            .build(),
    );
    let mut yielded = context.get_op(inner.id()).results().to_vec();
    yielded[port] = end.result();
    tile_block.append_op(scf::r#yield(context, yielded).build());
    let main = scf::ForOpBuilder::new(context)
        .lower_bound(lower)
        .upper_bound(last.result())
        .step(stride.result())
        .inits(carried.inits())
        .result_types(types.clone())
        .body(tile_region.id())
        .build();
    rewriter.insert_op_before(&target, &main)?;

    let left = context.get_op(main.id()).results().to_vec();
    let remainder = scf::ForOpBuilder::new(context)
        .lower_bound(left[port])
        .upper_bound(upper)
        .step(step)
        .inits(left)
        .result_types(types)
        .body(crate::clone::clone_region(context, body))
        .build();
    rewriter.insert_op_before(&target, &remainder)?;

    let results: Vec<ValueId> = context.get_op(remainder.id()).results().to_vec();
    for (&old, new) in handle.results().iter().zip(results) {
        context.replace_value_uses(old, new);
    }
    rewriter.erase_op(&target)?;
    Ok((main.id(), remainder.id()))
}
