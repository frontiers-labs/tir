//! Memory state threading.
//!
//! Memory order becomes an explicit def-use edge: the function's memory at entry
//! is named by `state.entry_state`, and every operation that touches memory
//! consumes the state it observes and produces the state it leaves behind. A slot
//! whose address never escapes is a memory of its own, so it gets a chain rooted
//! at its allocation; everything else — escaping slots, unknown pointers, calls —
//! shares one conservative chain, which the function's `return` exports.

use crate::BlockHandle;
use std::collections::{BTreeMap, BTreeSet};

use crate::analysis::AnalysisManager;
use crate::analysis::scopes::{exit_scope, loop_scope, nested_exit_scopes};
use crate::analysis::slots::collect_slots;
use crate::builtin::{CallOp, FuncOp, IndirectCallOp, ReturnOp, StateType};
use crate::ptr::MemcpyOp;
use crate::state::EntryStateOpBuilder;
use crate::{
    Context, MemoryRead, MemoryWrite, OpHandle, OpId, Operation, OperationRef, Pass, PassError,
    PassTarget, PromotableAllocation, RegionId, Rewriter, TypeId, ValueId, scf,
};

#[derive(Default)]
pub struct ThreadStatePass;

impl ThreadStatePass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(ThreadStatePass, "thread-state");

impl Pass for ThreadStatePass {
    fn name(&self) -> &'static str {
        "thread-state"
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
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        let block_ids = context.get_region(body).block_ids();
        let [entry] = block_ids[..] else {
            return Ok(());
        };

        let ops = region_ops(context, body);
        if !ops
            .iter()
            .any(|&op_id| touches_memory(&context.get_op(op_id)))
        {
            return Ok(());
        }

        let entry_block = context.get_block(entry);
        if !threadable(context, &entry_block) {
            return Ok(());
        }

        let tracked = collect_slots(context, &ops)
            .into_iter()
            .filter(|(_, slot)| slot.alloca.is_some() && !slot.escapes)
            .map(|(pointer, _)| pointer)
            .collect();

        let state = StateType::new(context);
        let root = EntryStateOpBuilder::new(context).result_type(state).build();
        entry_block.insert(0, root.id());

        Threader {
            context,
            state,
            tracked,
            chains: BTreeMap::from([(Chain::Conservative, root.result())]),
            scopes: Vec::new(),
        }
        .walk(&entry_block);

        Ok(())
    }
}

/// The memory a chain of state values describes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Chain {
    /// Everything that may alias: escaping slots, unknown pointers, and whatever a
    /// call touches.
    Conservative,
    /// One slot whose address never leaves the function, named by its allocation.
    Slot(ValueId),
}

/// How one operation relates to the state chains.
enum Effect {
    /// Opens a chain of its own.
    Open(ValueId),
    /// Observes a chain and leaves a new state behind.
    Access(Chain),
    /// Hands the conservative chain to the function's caller.
    Export,
}

struct Threader<'a> {
    context: &'a Context,
    state: TypeId,
    /// The slots that get a chain of their own.
    tracked: BTreeSet<ValueId>,
    /// The state each chain has reached at the point being walked.
    chains: BTreeMap<Chain, ValueId>,
    /// The token scopes of the loops whose ports are being grown, innermost last,
    /// each with the chains its exits carry, in port order.
    scopes: Vec<(ValueId, Vec<Chain>)>,
}

impl Threader<'_> {
    fn walk(&mut self, block: &BlockHandle) {
        for op_id in block.op_ids() {
            let op = self.context.get_op(op_id);
            match classify(&op, &self.tracked) {
                Some(Effect::Open(slot)) if self.tracked.contains(&slot) => {
                    let result = self.context.grow_port(op_id, self.state, None, |_, _| None);
                    self.chains.insert(Chain::Slot(slot), result);
                }
                Some(Effect::Access(chain)) => {
                    let observed = self.chain(chain);
                    let result =
                        self.context
                            .grow_port(op_id, self.state, Some(observed), |_, _| None);
                    self.chains.insert(chain, result);
                }
                Some(Effect::Export) => {
                    let exported = self.chain(Chain::Conservative);
                    self.context.append_operand(op_id, exported);
                }
                // An escaping slot is observable from anywhere, so it opens no
                // chain of its own: its accesses join the conservative one.
                Some(Effect::Open(_)) => {}
                // An exit leaves the loop through its carried ports, so it takes
                // the state each chain reached along that edge with it.
                None if self.exit_chains(&op).is_some() => {
                    for &chain in self.exit_chains(&op).expect("an exit of a walked loop") {
                        let leaving = self.chain(chain);
                        self.context.append_operand(op_id, leaving);
                    }
                }
                None if subtree_touches_memory(self.context, &op)
                    || !self.nested_exit_chains(&op).is_empty() =>
                {
                    self.thread_regions(&op)
                }
                None => {}
            }
        }
    }

    /// Carry every chain the op's regions touch across it as a port of its own:
    /// the state entering the op is the incoming operand, each region threads the
    /// argument it receives and carries the state it leaves out through its
    /// terminator, and the op's result is the state that reaches the code after.
    ///
    /// A loop carries the port across its iterations and a γ across its arms, but
    /// the edits are the same: what differs is only that a γ's arms are
    /// alternatives, so each threads its own copy of the state entering the gate
    /// and the result is whichever copy the arm that ran left behind.
    fn thread_regions(&mut self, op: &OpHandle) {
        let context = self.context;
        // A chain an exit inside carries out has to cross this op as a port too:
        // the exit takes the state its own copy reached, not the one the code
        // after the op observes.
        let exiting = self.nested_exit_chains(op);
        let carried = self
            .chains
            .keys()
            .copied()
            .filter(|&chain| self.touches_chain(op, chain) || exiting.contains(&chain))
            .collect::<Vec<_>>();

        let entries = op
            .regions()
            .iter()
            .map(|&region| context.get_block(context.get_region(region).block_ids()[0]))
            .collect::<Vec<_>>();

        let arguments = entries
            .iter()
            .map(|entry| {
                carried
                    .iter()
                    .map(|_| context.append_block_argument(entry.id(), self.state).id())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let scope = loop_scope(
            context,
            *op.regions().last().expect("a region to carry state"),
        );
        if let Some(scope) = scope {
            self.scopes.push((scope, carried.clone()));
        }

        for (entry, arguments) in entries.iter().zip(&arguments) {
            let outer = std::mem::replace(
                &mut self.chains,
                carried
                    .iter()
                    .copied()
                    .zip(arguments.iter().copied())
                    .collect(),
            );
            self.walk(entry);
            let leaving = carried
                .iter()
                .map(|&chain| self.chain(chain))
                .collect::<Vec<_>>();
            self.chains = outer;

            // A region that leaves through an exit fed the ports along that edge.
            let terminator =
                context.get_op(*entry.op_ids().last().expect("a region is terminated"));
            if self.exit_chains(&terminator).is_none() {
                for value in leaving {
                    context.append_operand(terminator.id, value);
                }
            }
        }

        if scope.is_some() {
            self.scopes.pop();
        }

        for &chain in &carried {
            let init = self.chain(chain);
            context.append_operand(op.id, init);
        }
        for &chain in &carried {
            // The ports the op already carries are wired up; all that is left is
            // the result naming the state it leaves behind.
            let result = context.grow_port(op.id, self.state, None, |_, _| None);
            self.chains.insert(chain, result);
        }
    }

    /// Whether anything inside `op`'s regions accesses `chain`.
    fn touches_chain(&self, op: &OpHandle, chain: Chain) -> bool {
        op.regions()
            .iter()
            .flat_map(|&region| region_ops(self.context, region))
            .any(|op_id| {
                matches!(
                    classify(&self.context.get_op(op_id), &self.tracked),
                    Some(Effect::Access(accessed)) if accessed == chain
                )
            })
    }

    /// The chains `op` carries out of a loop being walked, if it is such an exit.
    fn exit_chains(&self, op: &OpHandle) -> Option<&[Chain]> {
        let scope = exit_scope(op)?;
        self.scopes
            .iter()
            .find(|(token, _)| *token == scope)
            .map(|(_, chains)| chains.as_slice())
    }

    /// Every chain an exit inside `op`'s regions carries out.
    fn nested_exit_chains(&self, op: &OpHandle) -> BTreeSet<Chain> {
        nested_exit_scopes(self.context, op)
            .into_iter()
            .filter_map(|scope| self.scopes.iter().find(|(token, _)| *token == scope))
            .flat_map(|(_, chains)| chains.iter().copied())
            .collect()
    }

    fn chain(&self, chain: Chain) -> ValueId {
        *self
            .chains
            .get(&chain)
            .expect("the chain reaches this point")
    }
}

/// How `op` relates to the chains, reading an access through an untracked pointer
/// as one that may alias anything.
fn classify(op: &OpHandle, tracked: &BTreeSet<ValueId>) -> Option<Effect> {
    let chain_of = |pointer| {
        if tracked.contains(&pointer) {
            Chain::Slot(pointer)
        } else {
            Chain::Conservative
        }
    };
    if let Some(allocation) = op.clone().as_interface::<dyn PromotableAllocation>() {
        return Some(Effect::Open(allocation.promoted_location()));
    }
    if let Some(read) = op.clone().as_interface::<dyn MemoryRead>() {
        return Some(Effect::Access(chain_of(read.read_location())));
    }
    if let Some(write) = op.clone().as_interface::<dyn MemoryWrite>() {
        return Some(Effect::Access(chain_of(write.write_location())));
    }
    if op.is::<MemcpyOp>() || op.is::<CallOp>() || op.is::<IndirectCallOp>() {
        return Some(Effect::Access(Chain::Conservative));
    }
    op.is::<ReturnOp>().then_some(Effect::Export)
}

/// Whether `op` is one of the operations that carry state. Exporting the chain is
/// not touching memory: a `return` alone leaves a function with nothing to thread.
fn touches_memory(op: &OpHandle) -> bool {
    matches!(
        classify(op, &BTreeSet::new()),
        Some(Effect::Open(_) | Effect::Access(_))
    )
}

/// Whether every operation touching memory sits where a chain can reach it: a
/// chain crosses a region boundary as a port, which `scf`'s loops and gates have.
/// An op whose regions neither touch memory nor are left through an exit is
/// transparent and needs nothing; one that is left through an exit has to be
/// walked to feed that edge, so it is held to the same shape.
fn threadable(context: &Context, block: &BlockHandle) -> bool {
    block.op_ids().iter().all(|&op_id| {
        let op = context.get_op(op_id);
        if op.regions().is_empty()
            || !(subtree_touches_memory(context, &op)
                || !nested_exit_scopes(context, &op).is_empty())
        {
            return true;
        }
        if !carries_state(&op) {
            return false;
        }
        op.regions().iter().all(|&region| {
            let blocks = context.get_region(region).block_ids();
            let [entry] = blocks[..] else {
                return false;
            };
            threadable(context, &context.get_block(entry))
        })
    })
}

/// Whether `op` can carry a chain across its regions as a port.
fn carries_state(op: &OpHandle) -> bool {
    op.is::<scf::ForOp>()
        || op.is::<scf::WhileOp>()
        || op.is::<scf::IfOp>()
        || op.is::<scf::SwitchOp>()
}

fn subtree_touches_memory(context: &Context, op: &OpHandle) -> bool {
    op.regions()
        .iter()
        .flat_map(|&region| region_ops(context, region))
        .any(|op_id| touches_memory(&context.get_op(op_id)))
}

fn block_ops(context: &Context, block: &BlockHandle) -> Vec<OpId> {
    let mut ops = Vec::new();
    for op_id in block.op_ids() {
        ops.push(op_id);
        for region in context.get_op(op_id).regions() {
            ops.extend(region_ops(context, region));
        }
    }
    ops
}

/// Every operation in a region tree, outermost first.
fn region_ops(context: &Context, region: RegionId) -> Vec<OpId> {
    context
        .get_region(region)
        .iter(context.clone())
        .flat_map(|block| block_ops(context, &block))
        .collect()
}
