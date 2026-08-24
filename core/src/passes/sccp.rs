//! Sparse conditional constant propagation: materialize what
//! [`ConstantFacts`] proves constant and rewire every read of it to the literal.
//!
//! The pass makes one kind of edit. A value the solver pins to an integer gets a
//! `builtin.constant` built where the value was defined, and its readers are
//! rewired to that literal; the op that used to compute it is left for `dce`,
//! along with the blocks the solver never reached. Nothing here deletes.

use std::collections::HashMap;

use crate::analysis::{ConstantFacts, DefUse};
use crate::{
    AnalysisManager, BlockId, ConstantLike, Context, OpId, OperationRef, Pass, PassError,
    PassTarget, Rewriter, TypeId, ValueId, func::FuncOp,
};

#[derive(Default)]
pub struct SccpPass;

impl SccpPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(SccpPass, "sccp");

impl Pass for SccpPass {
    fn name(&self) -> &'static str {
        "sccp"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }
        let root = op.op().id;
        let facts = analyses.get::<ConstantFacts>(context, root);
        let defuse = analyses.get::<DefUse>(context, root);
        // The literals already built in a block, so a block that needs the same
        // one twice reads the one it has. Every literal is built ahead of the
        // value it replaces, so an earlier one dominates every later read.
        let mut built: HashMap<(BlockId, TypeId, i64), ValueId> = HashMap::new();

        for block in super::regions_under(context, root)
            .into_iter()
            .flat_map(|region| context.get_region(region).block_ids())
        {
            if !facts.is_executable(block) {
                continue;
            }
            let handle = context.get_block(block);
            let op_ids = handle.op_ids();
            let Some(&first) = op_ids.first() else {
                continue;
            };
            let mut rewrite = |value: ValueId, before: OpId, rewriter: &mut Rewriter| {
                if !defuse.is_used(value.number()) {
                    return Ok(());
                }
                materialize(context, rewriter, &facts, &mut built, value, before)
            };
            // A block argument is read inside the block it enters, so its
            // literal goes at the top of that block.
            for argument in handle.arguments() {
                rewrite(argument.id(), first, rewriter)?;
            }
            for op_id in op_ids {
                let instance = context.get_op(op_id);
                // A literal already states what it holds.
                if instance
                    .clone()
                    .as_interface::<dyn ConstantLike>()
                    .is_some()
                {
                    continue;
                }
                for result in instance.results().to_vec() {
                    rewrite(result, op_id, rewriter)?;
                }
            }
        }
        Ok(())
    }
}

/// Build the literal `value` holds ahead of `before`, and rewire its readers to
/// it. A value nothing proved constant is left alone.
fn materialize(
    context: &Context,
    rewriter: &mut Rewriter,
    facts: &ConstantFacts,
    built: &mut HashMap<(BlockId, TypeId, i64), ValueId>,
    value: ValueId,
    before: OpId,
) -> Result<(), PassError> {
    let Some(constant) = facts.constant(value) else {
        return Ok(());
    };
    let ty = context.get_value(value).ty();
    let Some(block) = context.parent_block(before) else {
        return Ok(());
    };
    let key = (block, ty, constant.to_i64());
    let literal = match built.get(&key) {
        Some(&literal) => literal,
        None => {
            let target =
                OperationRef::new(context.get_op(before), Some(context.get_block(block)), None);
            let literal = super::literal_before(context, rewriter, constant.to_i64(), ty, &target)?;
            built.insert(key, literal);
            literal
        }
    };
    context.replace_value_uses(value, literal);
    Ok(())
}
