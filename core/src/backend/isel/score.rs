//! What a selected function costs before register allocation.

use tir::{Context, OpHandle, OpId};

use super::rule::LATENCY_COST_SCALE;

/// What a prepared function costs before register allocation. Scores order by
/// priced cost first; at equal cost the plan leaving fewer operations of
/// unknown cost wins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PlanScore {
    /// Every recovered machine instruction at [`LATENCY_COST_SCALE`] times its
    /// latency plus its encoding size, counted once.
    cost: u64,
    /// Operations later stages still lower or place: calls, returns, constants
    /// kept for the target's materializer and virtual branches. Spills, final
    /// layout and branch relaxation are not known here either.
    unpriced: u32,
}

impl PlanScore {
    pub(super) fn of(context: &Context, function: OpId) -> Self {
        let mut score = Self::default();
        for region in crate::passes::regions_under(context, function) {
            for op in context.get_region(region).op_ids() {
                let op = context.get_op(op);
                if !op.regions().is_empty() {
                    continue;
                }
                match instruction_cost(&op) {
                    Some(cost) => score.cost += cost,
                    None => score.unpriced += 1,
                }
            }
        }
        score
    }
}

fn instruction_cost(op: &OpHandle) -> Option<u64> {
    op.clone()
        .as_interface::<dyn crate::backend::MachineInstruction>()
        .map(|instruction| instruction.info())
        .filter(|info| info.width_bytes.1 > 0)
        .map(|info| u64::from(info.cost * LATENCY_COST_SCALE + u32::from(info.width_bytes.0)))
}
