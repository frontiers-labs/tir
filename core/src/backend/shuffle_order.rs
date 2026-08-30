//! Re-linearization of a machine block under its dependence edges.
//!
//! The backend half of the mid-end's `shuffle-state` oracle. Order inside a
//! machine block is one topological order of the block's dependence DAG
//! ([`Dependences`]); this pass picks another one, seeded, so a dependence the
//! graph does not spell shows up as a behavior change rather than as a
//! reviewer's doubt. It is an oracle, not an optimization: nothing in a
//! production pipeline runs it.
//!
//! It runs twice — once after selection, where a value is its own resource, and
//! once after allocation, where the map turns values into registers and the
//! graph carries the anti- and output edges that come with them.

use tir::{
    AnalysisManager, Context, OperationRef, Pass, PassError, PassTarget, Rewriter, utils::Rng,
};

use crate::backend::{ASSIGNMENT_ATTR, Dependences, RegAssignment, SymbolOp};

pub struct ShuffleMachineOrderPass {
    rng: Rng,
}

impl ShuffleMachineOrderPass {
    pub fn new() -> Self {
        Self {
            rng: Rng::from_environment(),
        }
    }
}

impl Default for ShuffleMachineOrderPass {
    fn default() -> Self {
        Self::new()
    }
}

crate::register_pass!(ShuffleMachineOrderPass, "shuffle-machine-order");

impl Pass for ShuffleMachineOrderPass {
    fn name(&self) -> &'static str {
        "shuffle-machine-order"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<SymbolOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let assignment = RegAssignment::of_op(op.op(), ASSIGNMENT_ATTR);
        for region in op.op().regions().to_vec() {
            for block in context.get_region(region).iter(context.clone()) {
                let ops = block.op_ids();
                if ops.len() < 2 {
                    continue;
                }
                let graph = Dependences::of_ops(context, &ops, &assignment);
                let order = graph.shuffle(self.rng.next_u64()).ok_or_else(|| {
                    PassError::InvalidRuleSet(format!("cyclic dependences in {:?}", block.id()))
                })?;
                block.set_ops(order);
            }
        }
        Ok(())
    }
}
