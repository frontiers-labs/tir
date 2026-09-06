//! Memory order constructed over ordered blocks, before the order is gone.
//!
//! One chain: every effect observes the memory the last change left and leaves
//! a dependency of its own. Reads fork off a change without ordering one
//! another; the next change, or whatever leaves the block, takes `state.join`
//! of what the fork left, so a read never trails the write that overtakes it.

use crate::func::CallOp;
use crate::ptr::MemcpyOp;
use crate::state::JoinOpBuilder;
use crate::{
    BlockId, Context, MemoryRead, MemoryWrite, OpHandle, OpId, Operation, PassError, ValueId, scf,
};

use super::cfg::unsupported;

/// Whether `region` needs a chain constructed: something in it touches memory,
/// and nothing already names a dependency, which would make a second order
/// over the one that is there.
pub fn wants_chain(context: &Context, region: crate::RegionId) -> bool {
    let ops: Vec<OpHandle> = crate::analysis::scopes::region_ops(context, region)
        .into_iter()
        .map(|op| context.get_op(op))
        .collect();
    let threaded = ops
        .iter()
        .any(|op| !op.dep_operands().is_empty() || !op.dep_results().is_empty());
    !threaded
        && ops
            .iter()
            .any(|op| !matches!(effect(context, op), Ok(Effect::None)))
}

/// Thread `block`'s operations, its terminator excluded, off `entry`, and
/// answer the memory the block leaves with. Joins go before the operation that
/// takes them.
pub fn thread_block(
    context: &Context,
    block: BlockId,
    entry: ValueId,
) -> Result<ValueId, PassError> {
    let ops = context.get_block(block).op_ids();
    let (&terminator, body) = ops
        .split_last()
        .ok_or_else(|| unsupported("a block with no terminator"))?;
    let mut chain = Chain {
        context,
        written: entry,
        reads: Vec::new(),
    };
    for &op in body {
        let handle = context.get_op(op);
        match effect(context, &handle)? {
            Effect::None => {}
            Effect::Read => {
                context.append_dep_operand(op, chain.written);
                let left = context.append_dep_result(op);
                chain.reads.push(left);
            }
            Effect::Change => {
                let observed = chain.settle(op);
                context.append_dep_operand(op, observed);
                chain.written = context.append_dep_result(op);
            }
            Effect::CountedLoop => {
                let observed = chain.settle(op);
                let body = handle.regions()[0];
                let [body_block] = context.get_region(body).block_ids()[..] else {
                    return Err(unsupported("a counted loop whose body is a graph"));
                };
                let port = context.append_dep_block_argument(body_block).id();
                let leaving = thread_block(context, body_block, port)?;
                let latch = *context.get_block(body_block).op_ids().last().unwrap();
                context.append_dep_operand(latch, leaving);
                context.append_dep_operand(op, observed);
                chain.written = context.append_dep_result(op);
            }
        }
    }
    Ok(chain.settle(terminator))
}

enum Effect {
    None,
    Read,
    Change,
    /// An `scf.for` the frontend raised: its body is one block, threaded off
    /// a dependency port the loop carries.
    CountedLoop,
}

fn effect(context: &Context, op: &OpHandle) -> Result<Effect, PassError> {
    if op.has_interface::<dyn MemoryWrite>() || op.is::<MemcpyOp>() || op.is::<CallOp>() {
        return Ok(Effect::Change);
    }
    if op.has_interface::<dyn MemoryRead>() {
        return Ok(Effect::Read);
    }
    if op.is::<scf::ForLegacyOp>() {
        return Ok(Effect::CountedLoop);
    }
    let nested = op
        .regions()
        .iter()
        .flat_map(|&region| crate::analysis::scopes::region_ops(context, region))
        .any(|inner| !matches!(effect(context, &context.get_op(inner)), Ok(Effect::None)));
    if nested {
        return Err(unsupported(&format!(
            "memory effects inside {}.{}",
            op.dialect(),
            op.name()
        )));
    }
    Ok(Effect::None)
}

struct Chain<'a> {
    context: &'a Context,
    written: ValueId,
    reads: Vec<ValueId>,
}

impl Chain<'_> {
    /// The memory after every read of the open fork: the last change where none
    /// forked off it, the one read's state where one did, their join otherwise.
    fn settle(&mut self, before: OpId) -> ValueId {
        let reads = std::mem::take(&mut self.reads);
        self.written = match reads.len() {
            0 => self.written,
            1 => reads[0],
            _ => {
                let mut join = JoinOpBuilder::new(self.context).dep_result();
                for read in reads {
                    join = join.dep_operand(read);
                }
                let join = join.build();
                let block = self
                    .context
                    .get_block(self.context.parent_block(before).expect("an op in a block"));
                let at = block
                    .op_ids()
                    .iter()
                    .position(|&op| op == before)
                    .expect("the op sits in its block");
                block.insert(at, join.id());
                join.result()
            }
        };
        self.written
    }
}
