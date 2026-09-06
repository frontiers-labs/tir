//! Re-linearization of a block under its dependence edges.
//!
//! Memory order is the state DAG (`ir.md` §6.3): the order a region spells is
//! one linearization of it, and any other is the same program. This pass picks a
//! different one — a seeded random topological order of the value and state
//! edges — so a missing edge shows up as a behavior change rather than as a
//! reviewer's doubt. It is an oracle, not an optimization: nothing in a
//! production pipeline runs it.
//!
//! Only a block whose every operation the edges describe is re-linearized. An
//! operation naming a state is ordered by the chain it observes, and a pure one
//! by its operands; anything else — an effect no edge spells — pins its block,
//! because moving it would change a program the representation does not claim
//! to order.
//!
//! An unordered region has no order to pin: its insertion order is never read,
//! so any permutation of it is the same region, and it gets one.

use crate::func::FuncOp;
use crate::utils::Rng;
use crate::{
    AnalysisManager, BlockHandle, Context, OpHandle, OpId, OperationRef, Pass, PassError,
    PassTarget, RegionId, Rewriter, ValueId,
};
use std::collections::HashMap;

pub struct ShuffleStatePass {
    rng: Rng,
}

impl ShuffleStatePass {
    pub fn new() -> Self {
        Self {
            rng: Rng::from_environment(),
        }
    }
}

impl Default for ShuffleStatePass {
    fn default() -> Self {
        Self::new()
    }
}

crate::register_pass!(ShuffleStatePass, "shuffle-state");

impl Pass for ShuffleStatePass {
    fn name(&self) -> &'static str {
        "shuffle-state"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        for region in op.op().regions().to_vec() {
            self.shuffle_region(context, region);
        }
        Ok(())
    }
}

impl ShuffleStatePass {
    fn shuffle_region(&mut self, context: &Context, region: RegionId) {
        let handle = context.get_region(region);
        if handle.is_nodes() {
            let ops = handle.op_ids();
            for &op_id in &ops {
                for nested in context.get_op(op_id).regions() {
                    self.shuffle_region(context, nested);
                }
            }
            self.shuffle_nodes(context, region, ops);
            return;
        }
        for block in handle.iter(context.clone()) {
            for op_id in block.op_ids() {
                for nested in context.get_op(op_id).regions() {
                    self.shuffle_region(context, nested);
                }
            }
            self.shuffle_block(context, &block);
        }
    }

    /// Insert `ops` into `region` again, in another order.
    fn shuffle_nodes(&mut self, context: &Context, region: RegionId, mut ops: Vec<OpId>) {
        for &op_id in &ops {
            context.remove_from_region(region, op_id);
        }
        while !ops.is_empty() {
            let picked = ops.swap_remove(self.rng.below(ops.len()));
            context.add(region, picked);
        }
    }

    /// Give `block` another order of the same edges. The terminator stays last:
    /// what leaves a region is not a scheduling question.
    fn shuffle_block(&mut self, context: &Context, block: &BlockHandle) {
        let op_ids = block.op_ids();
        let Some((&terminator, body)) = op_ids.split_last() else {
            return;
        };
        if body.len() < 2 {
            return;
        }
        if !body
            .iter()
            .all(|&op_id| ordered_by_edges(context, &context.get_op(op_id)))
        {
            return;
        }

        let mut pending = dependency_counts(context, body);
        let mut ready: Vec<usize> = (0..body.len()).filter(|&i| pending[i].0 == 0).collect();
        let mut order = Vec::with_capacity(body.len());
        while !ready.is_empty() {
            let picked = ready.swap_remove(self.rng.below(ready.len()));
            order.push(body[picked]);
            for user in std::mem::take(&mut pending[picked].1) {
                pending[user].0 -= 1;
                if pending[user].0 == 0 {
                    ready.push(user);
                }
            }
        }
        debug_assert_eq!(order.len(), body.len(), "the dependences form a DAG");

        for op_id in order.into_iter().chain([terminator]) {
            block.remove_op(op_id);
            block.append(op_id);
        }
    }
}

/// How many operations of the block each one waits for, and which ones wait for
/// it. An operation waits for every definition its own subtree reads: a region
/// it holds names the values around it, and it can only run where they do.
fn dependency_counts(context: &Context, body: &[OpId]) -> Vec<(usize, Vec<usize>)> {
    let defined: HashMap<ValueId, usize> = body
        .iter()
        .enumerate()
        .flat_map(|(index, &op_id)| {
            context
                .get_op(op_id)
                .results()
                .to_vec()
                .into_iter()
                .map(move |result| (result, index))
        })
        .collect();
    let mut counts = vec![(0, Vec::new()); body.len()];
    for (index, &op_id) in body.iter().enumerate() {
        let mut seen = Vec::new();
        for operand in subtree_operands(context, op_id) {
            let Some(&producer) = defined.get(&operand) else {
                continue;
            };
            if producer == index || seen.contains(&producer) {
                continue;
            }
            seen.push(producer);
            counts[index].0 += 1;
            counts[producer].1.push(index);
        }
    }
    counts
}

/// Every value read by `op` or by anything under it.
fn subtree_operands(context: &Context, op: OpId) -> Vec<ValueId> {
    let instance = context.get_op(op);
    let mut operands = instance.operands().to_vec();
    for region in instance.regions() {
        for block in context.get_region(region).iter(context.clone()) {
            for nested in block.op_ids() {
                operands.extend(subtree_operands(context, nested));
            }
        }
    }
    operands
}

/// Whether the edges say where `op` may run: it opens a memory, so nothing does;
/// it observes a dependency, so the chain does; or it is a value the vocabulary
/// spells, so its operands do. An operation holding regions is read through
/// them — one whose regions carry no dependency and hold nothing else touches
/// no memory at all.
fn ordered_by_edges(context: &Context, op: &OpHandle) -> bool {
    // An allocation opens a memory of its own, so nothing orders it: what reaches
    // into that memory names the pointer it defines, and that edge is enough.
    if op.has_interface::<dyn crate::PromotableAllocation>() {
        return true;
    }
    if !op.dep_operands().is_empty() || !op.dep_results().is_empty() {
        return true;
    }
    if super::is_pure_value(op) {
        return true;
    }
    !op.regions().is_empty()
        && op.regions().iter().all(|&region| {
            context
                .get_region(region)
                .iter(context.clone())
                .all(|block| {
                    block
                        .op_ids()
                        .iter()
                        .all(|&nested| ordered_by_edges(context, &context.get_op(nested)))
                })
        })
}
