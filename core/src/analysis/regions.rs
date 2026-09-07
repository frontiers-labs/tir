//! Walking a region tree: the operations a region holds, at any depth.

use crate::{Context, OpHandle, OpId, RegionId};

/// Every operation in `region`'s tree, outermost first: each op precedes the
/// ops of its own regions.
pub fn region_ops(context: &Context, region: RegionId) -> Vec<OpId> {
    let mut ops = Vec::new();
    for op_id in context.get_region(region).op_ids() {
        ops.push(op_id);
        for nested in context.get_op(op_id).regions() {
            ops.extend(region_ops(context, nested));
        }
    }
    ops
}

/// Every operation under `op`'s regions, outermost first.
pub fn subtree_ops(context: &Context, op: &OpHandle) -> Vec<OpId> {
    op.regions()
        .iter()
        .flat_map(|&region| region_ops(context, region))
        .collect()
}
