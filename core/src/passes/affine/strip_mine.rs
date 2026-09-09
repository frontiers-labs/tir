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

use crate::attributes::Predicate;
use crate::builtin::ops as b;
use crate::{
    Context, CountedLoop, OpId, Operation, OperationRef, PassError, RegionId, Theta, TypeId, Value,
    ValueId, scf,
};

/// Replace `op` with its tiled form: the loop over whole tiles of `tile`
/// iterations, then the loop over the remainder, in that order.
///
/// The loop carries its counter and its chains alone: the tile loop's body is
/// a graph holding the inner loop, and the remainder loop is entered on the
/// counter the tile loop ended at.
pub fn strip_mine(context: &Context, op: OpId, tile: i128) -> Result<(OpId, OpId), PassError> {
    let Some(parent) = context.parent_nodes_region(op) else {
        return Err(PassError::RewriteFailed(op));
    };
    let handle = context.get_op(op);
    let (Some(counted), Some(theta)) = (
        handle.clone().as_interface::<dyn CountedLoop>(),
        handle.clone().as_interface::<dyn Theta>(),
    ) else {
        return Err(PassError::RewriteFailed(op));
    };
    let body = theta.body();
    let states = crate::binding::state_chains(context, &handle);
    if handle.value_results().len() != 1 || theta.binding().ports.len() != 1 + states.len() {
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
            let mut result_types = vec![ty];
            result_types.extend(states.iter().map(|_| TypeId::STATE));
            scf::ForOpBuilder::new(context)
                .lb(lower)
                .inits(states.to_vec())
                .ub(upper)
                .step(step)
                .body(body)
                .result_types(result_types)
                .build()
        };

    let base = context.create_value(ty, None);
    let mut ports = vec![base.clone()];
    ports.extend(
        states
            .iter()
            .map(|_| context.create_value(TypeId::STATE, None)),
    );
    let dep_ports: Vec<ValueId> = ports[1..].iter().map(Value::id).collect();
    let tile_body = context.create_nodes_region(ports, vec![], vec![]).id();
    let end = b::addi(context, base.id(), stride.result(), ty).build();
    context.add(tile_body, end.id());
    let inner_body = crate::clone::clone_region(context, body);
    retarget_predicate(context, inner_body, end.result());
    let inner = counted_loop(inner_body, base.id(), end.result(), step, &dep_ports);
    context.add(tile_body, inner.id());
    let boolean = crate::builtin::IntegerType::new(context, 1);
    let compare = b::cmpi(context, base.id(), last.result(), Predicate::Slt, boolean).build();
    context.add(tile_body, compare.id());
    let mut results = vec![compare.result(), end.result()];
    results.extend(context.get_op(inner.id()).state_results());
    results.push(base.id());
    results.extend(dep_ports);
    context.set_region_results(tile_body, results);

    let main = counted_loop(
        tile_body,
        lower,
        last.result(),
        stride.result(),
        &handle.state_operands(),
    );
    place(main.id());
    let main_handle = context.get_op(main.id());
    let remainder = counted_loop(
        crate::clone::clone_region(context, body),
        main_handle.value_results()[0],
        upper,
        step,
        &main_handle.state_results(),
    );
    place(remainder.id());

    let results = context.get_op(remainder.id()).results().to_vec();
    for (&old, new) in handle.results().iter().zip(results) {
        context.replace_value_uses(old, new);
        context.rename_region_results(parent, old, new, &[]);
    }
    context.erase_op(&OperationRef::new(handle))?;
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
