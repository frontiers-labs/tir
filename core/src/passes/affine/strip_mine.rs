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
use crate::attributes::Predicate;
use crate::builtin::ops as b;
use crate::{
    Context, CountedLoop, LoopLike, OpId, Operation, OperationRef, PassError, RegionId, Rewriter,
    Theta, TypeId, Value, ValueId, scf,
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
    if let Some(region) = context.parent_nodes_region(op) {
        return strip_mine_nodes(context, rewriter, op, region, tile);
    }
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
    let deps = handle.dep_results().len();
    let types: Vec<TypeId> = handle
        .value_results()
        .iter()
        .map(|&result| context.get_value(result).ty())
        .collect();
    // A body opening a token scope leaves the loop through a `break`, which a
    // copy cannot stand for.
    if context
        .get_block(context.get_region(body).block_ids()[0])
        .arguments()
        .len()
        != types.len() + deps
    {
        return Err(PassError::RewriteFailed(op));
    }
    let target = OperationRef::new(handle.clone());
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
        .chain((0..deps).map(|_| context.create_value(TypeId::DEPENDENCY, None)))
        .collect();
    let tile_block = context.create_block_with_dependencies(tile_args.clone(), deps);
    tile_region.add_block(tile_block.id());
    let base = tile_args[port].id();
    let end = tile_block.append_op(b::addi(context, base, stride.result(), ty).build());
    let ports = |ports: &[ValueId], body: crate::RegionId, lower, upper, step| {
        let (values, deps) = ports.split_at(ports.len() - deps);
        let mut builder = scf::ForLegacyOpBuilder::new(context)
            .lower_bound(lower)
            .upper_bound(upper)
            .step(step)
            .inits(values.to_vec())
            .result_types(types.clone())
            .body(body);
        for &dep in deps {
            builder = builder.dep_operand(dep).dep_result();
        }
        builder.build()
    };
    let inner = tile_block.append_op(ports(
        &tile_args.iter().map(Value::id).collect::<Vec<_>>(),
        crate::clone::clone_region(context, body),
        base,
        end.result(),
        step,
    ));
    let mut yielded = context.get_op(inner.id()).results().to_vec();
    yielded[port] = end.result();
    let (values, dep_values) = yielded.split_at(yielded.len() - deps);
    let mut yield_op = scf::r#yield(context, values.to_vec());
    for &dep in dep_values {
        yield_op = yield_op.dep_operand(dep);
    }
    tile_block.append_op(yield_op.build());
    let main = ports(
        &carried.inits(),
        tile_region.id(),
        lower,
        last.result(),
        stride.result(),
    );
    rewriter.insert_op_before(&target, &main)?;

    let left = context.get_op(main.id()).results().to_vec();
    let remainder = ports(
        &left,
        crate::clone::clone_region(context, body),
        left[port],
        upper,
        step,
    );
    rewriter.insert_op_before(&target, &remainder)?;

    let results: Vec<ValueId> = context.get_op(remainder.id()).results().to_vec();
    for (&old, new) in handle.results().iter().zip(results) {
        context.replace_value_uses(old, new);
    }
    rewriter.erase_op(&target)?;
    Ok((main.id(), remainder.id()))
}

/// [`strip_mine`] for an unordered counted loop carrying its counter and its
/// chains alone: the tile loop's body is a graph holding the inner loop, and
/// the remainder loop is entered on the counter the tile loop ended at.
fn strip_mine_nodes(
    context: &Context,
    rewriter: &mut Rewriter,
    op: OpId,
    parent: RegionId,
    tile: i128,
) -> Result<(OpId, OpId), PassError> {
    let handle = context.get_op(op);
    let (Some(counted), Some(theta)) = (
        handle.clone().as_interface::<dyn CountedLoop>(),
        handle.clone().as_interface::<dyn Theta>(),
    ) else {
        return Err(PassError::RewriteFailed(op));
    };
    let body = theta.body();
    let deps = handle.dep_results().len();
    if handle.value_results().len() != 1 || theta.carried().ports.len() != 1 {
        return Err(PassError::RewriteFailed(op));
    }
    let (lower, upper, step) = (counted.lower_bound(), counted.upper_bound(), counted.step());
    let ty = context.get_value(lower).ty();
    let place = |op: OpId| context.add(parent, op);

    let tile = b::constant(context, tile as i64, ty).build();
    place(tile.id());
    let stride = b::muli(context, step, tile.result(), ty).build();
    place(stride.id());
    let span = b::subi(context, upper, lower, ty).build();
    place(span.id());
    let tiles = b::divui(context, span.result(), stride.result(), ty).build();
    place(tiles.id());
    let covered = b::muli(context, tiles.result(), stride.result(), ty).build();
    place(covered.id());
    let last = b::addi(context, lower, covered.result(), ty).build();
    place(last.id());

    let counted_loop =
        |body: RegionId, lower: ValueId, upper: ValueId, step: ValueId, states: &[ValueId]| {
            let mut builder = scf::ForOpBuilder::new(context)
                .lb(lower)
                .inits(vec![])
                .ub(upper)
                .step(step)
                .body(body)
                .result_types(vec![ty]);
            for &state in states {
                builder = builder.dep_operand(state).dep_result();
            }
            builder.build()
        };

    let base = context.create_value(ty, None);
    let mut ports = vec![base.clone()];
    ports.extend((0..deps).map(|_| context.create_value(TypeId::DEPENDENCY, None)));
    let dep_ports: Vec<ValueId> = ports[1..].iter().map(Value::id).collect();
    let tile_body = context
        .create_nodes_region(ports, deps, vec![], vec![], 0)
        .id();
    let end = b::addi(context, base.id(), stride.result(), ty).build();
    context.add(tile_body, end.id());
    let inner_body = crate::clone::clone_region(context, body);
    retarget_predicate(context, inner_body, end.result());
    let inner = counted_loop(inner_body, base.id(), end.result(), step, &dep_ports);
    context.add(tile_body, inner.id());
    let boolean = crate::builtin::IntegerType::new(context, 1);
    let compare = b::cmpi(context, base.id(), last.result(), Predicate::Slt, boolean).build();
    context.add(tile_body, compare.id());
    let mut results = vec![compare.result(), end.result(), base.id()];
    results.extend(context.get_op(inner.id()).dep_results());
    results.extend(dep_ports);
    context.set_region_results(tile_body, results, 2 * deps);

    let main = counted_loop(
        tile_body,
        lower,
        last.result(),
        stride.result(),
        &handle.dep_operands(),
    );
    place(main.id());
    let main_handle = context.get_op(main.id());
    let remainder = counted_loop(
        crate::clone::clone_region(context, body),
        main_handle.value_results()[0],
        upper,
        step,
        &main_handle.dep_results(),
    );
    place(remainder.id());

    let results = context.get_op(remainder.id()).results().to_vec();
    for (&old, new) in handle.results().iter().zip(results) {
        context.replace_value_uses(old, new);
        context.rename_region_results(parent, old, new, &[]);
    }
    rewriter.erase_op(&OperationRef::new(handle))?;
    Ok((main.id(), remainder.id()))
}

/// Point a copied counted body's predicate at the bound its new loop counts
/// to: the copy compares the counter with the bound the original had.
fn retarget_predicate(context: &Context, body: RegionId, upper: ValueId) {
    let predicate = context.get_region(body).value_results()[0];
    if let Some(compare) = context.get_value(predicate).defining_op() {
        context.set_op_operand(compare, 1, upper);
    }
}
