//! Memory state threading.
//!
//! Memory order becomes an explicit def-use edge: the function's memory at entry
//! is named by `state.entry_state`, and every operation that touches memory
//! consumes the state it observes and produces the state it leaves behind.
//!
//! One chain per object. Every base object [`AliasFacts`] tells apart from all the
//! others the function names — a global, a parameter, a stack slot — is a memory
//! of its own, so accesses to it are ordered against each other and against
//! nothing else. A pointer the facts cannot read back may name any object they
//! cannot rule it out of, so where the function holds one every object but the
//! private slots shares the conservative chain, and so does every access through
//! such a pointer.
//!
//! A call and a `memcpy` touch every object the outside can reach, and a `return`
//! hands them all to the caller. Those chains cross such an operation through one
//! port: `state.join` merges them into the state it observes and `state.split`
//! names each of them again in the state it leaves. A slot whose address never
//! leaves the function is not among them.
//!
//! The edges are the whole order and no more than it. A read leaves memory as it
//! found it, so any number of reads observe the state one write left — a fork,
//! unordered among themselves — and the next write, call, export or carried port
//! on that chain takes `state.join` of what the fork left. RAW and WAW are the
//! chain edge, WAR is the join edge, and two reads of one state are ordered by
//! nothing.

use crate::BlockHandle;
use crate::analysis::scopes;
use std::collections::{BTreeMap, BTreeSet};

use crate::analysis::scopes::{exit_scope, loop_scope, nested_exit_scopes};
use crate::analysis::{AliasFacts, AnalysisManager, Base, PointerFact};
use crate::func::{CallOp, FuncOp, ReturnOp};
use crate::ptr::MemcpyOp;
use crate::state::{EntryStateOpBuilder, JoinOpBuilder, SplitOpBuilder};
use crate::{
    Context, MemoryRead, MemoryWrite, OpHandle, OpId, Operation, OperationRef, Pass, PassError,
    PassTarget, PromotableAllocation, RegionKind, Rewriter, ValueId, scf,
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
        PassTarget::operation_on::<FuncOp>(RegionKind::Blocks)
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        let block_ids = context.get_region(body).block_ids();
        let [entry] = block_ids[..] else {
            return Ok(());
        };

        let ops = scopes::region_ops(context, body);
        if !ops
            .iter()
            .any(|&op_id| touches_memory(&context.get_op(op_id)))
        {
            return Ok(());
        }
        // Threading a threaded function would draw a second order over the one
        // that is already there. The chains are the memory order, so where they
        // exist there is nothing to say.
        if already_threaded(context, &ops) {
            return Ok(());
        }

        let entry_block = context.get_block(entry);
        if !threadable(context, &entry_block) {
            return Ok(());
        }

        let facts = analyses.get::<AliasFacts>(context, op.op().id);
        let objects = Objects {
            distinguished: distinguished(context, &facts, &ops),
            facts: &facts,
        };

        // Every chain but a slot's starts at the memory the function was entered
        // with; a slot's starts where it is allocated.
        let mut chains = BTreeMap::new();
        let rooted_at_entry = std::iter::once(Chain::Conservative).chain(
            objects
                .distinguished
                .iter()
                .copied()
                .filter(|base| !matches!(base, Base::Alloca(_)))
                .map(Chain::Object),
        );
        for (index, chain) in rooted_at_entry.enumerate() {
            let root = EntryStateOpBuilder::new(context).dep_result().build();
            entry_block.insert(index, root.id());
            chains.insert(chain, ChainState::opened(root.result()));
        }

        Threader {
            context,
            objects,
            chains,
            scopes: Vec::new(),
        }
        .walk(&entry_block);

        Ok(())
    }
}

/// The memory a chain of state values describes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Chain {
    /// Everything the facts cannot tell apart: objects that may alias one
    /// another, and whatever a pointer they cannot read back names.
    Conservative,
    /// One object no other object of the function is.
    Object(Base),
}

/// What one operation does to memory, before the objects it names are read.
enum Kind {
    /// Opens the memory of a slot.
    Open(ValueId),
    /// Observes the memory an address names and leaves it as it found it.
    Read(ValueId),
    /// Leaves a memory at an address that the reads after it see.
    Write(ValueId),
    /// Touches every object the outside can reach.
    Clobber,
    /// Hands every object the outside can reach to the function's caller.
    Export,
}

/// Where one chain has got to: the memory the last write left, and the states the
/// reads observing it have left behind since.
struct ChainState {
    written: ValueId,
    reads: Vec<ValueId>,
}

impl ChainState {
    fn opened(written: ValueId) -> Self {
        Self {
            written,
            reads: Vec::new(),
        }
    }
}

/// The objects the facts tell apart, and the chain an address falls on.
struct Objects<'a> {
    facts: &'a AliasFacts,
    distinguished: BTreeSet<Base>,
}

impl Objects<'_> {
    fn chain_of(&self, address: ValueId) -> Chain {
        match self.facts.fact(address) {
            PointerFact::Object { base, .. } if self.distinguished.contains(&base) => {
                Chain::Object(base)
            }
            _ => Chain::Conservative,
        }
    }

    /// Whether nothing outside the function can reach `chain`, so neither a call
    /// nor the caller after a return observes it.
    fn is_private(&self, chain: Chain) -> bool {
        matches!(chain, Chain::Object(base) if self.facts.is_private(base))
    }
}

struct Threader<'a> {
    context: &'a Context,
    objects: Objects<'a>,
    /// The state each chain has reached at the point being walked.
    chains: BTreeMap<Chain, ChainState>,
    /// The token scopes of the loops whose ports are being grown, innermost last,
    /// each with the chains its exits carry, in port order.
    scopes: Vec<(ValueId, Vec<Chain>)>,
}

impl Threader<'_> {
    fn walk(&mut self, block: &BlockHandle) {
        for op_id in block.op_ids() {
            let op = self.context.get_op(op_id);
            match classify(&op) {
                Some(Kind::Open(slot)) if self.opens(slot) => {
                    let result = self.context.grow_dep_port(op_id, None, |_, _| None);
                    self.chains.insert(
                        Chain::Object(Base::Alloca(slot)),
                        ChainState::opened(result),
                    );
                }
                Some(Kind::Read(address)) => {
                    let chain = self.chain(address);
                    let observed = self.chains[&chain].written;
                    let result = self
                        .context
                        .grow_dep_port(op_id, Some(observed), |_, _| None);
                    self.chain_mut(chain).reads.push(result);
                }
                Some(Kind::Write(address)) => {
                    let chain = self.chain(address);
                    let observed = self.settle(chain, op_id);
                    let result = self
                        .context
                        .grow_dep_port(op_id, Some(observed), |_, _| None);
                    self.chains.insert(chain, ChainState::opened(result));
                }
                Some(Kind::Clobber) => {
                    let exposed = self.exposed();
                    let observed = self.merge(&exposed, op_id);
                    let result = self
                        .context
                        .grow_dep_port(op_id, Some(observed), |_, _| None);
                    self.spread(&exposed, result, op_id);
                }
                Some(Kind::Export) => {
                    let exposed = self.exposed();
                    let exported = self.merge(&exposed, op_id);
                    self.context.append_dep_operand(op_id, exported);
                }
                // A slot the facts cannot tell from another object opens no chain
                // of its own: its accesses join the conservative one.
                Some(Kind::Open(_)) => {}
                // An exit leaves the loop through its carried ports, so it takes
                // the state each chain reached along that edge with it.
                None if self.exit_chains(&op).is_some() => {
                    let chains = self
                        .exit_chains(&op)
                        .expect("an exit of a walked loop")
                        .to_vec();
                    for chain in chains {
                        let leaving = self.settle(chain, op_id);
                        self.context.append_dep_operand(op_id, leaving);
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
        let inside = self.chains_touched_under(op);
        let carried = self
            .chains
            .keys()
            .copied()
            .filter(|chain| inside.contains(chain) || exiting.contains(chain))
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
                    .map(|_| context.append_dep_block_argument(entry.id()).id())
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
                    .zip(arguments.iter().copied().map(ChainState::opened))
                    .collect(),
            );
            self.walk(entry);
            let terminator = *entry.op_ids().last().expect("a region is terminated");
            let leaving = carried
                .iter()
                .map(|&chain| self.settle(chain, terminator))
                .collect::<Vec<_>>();
            self.chains = outer;

            // A region that leaves through an exit fed the ports along that edge.
            if self.exit_chains(&context.get_op(terminator)).is_none() {
                for value in leaving {
                    context.append_dep_operand(terminator, value);
                }
            }
        }

        if scope.is_some() {
            self.scopes.pop();
        }

        for &chain in &carried {
            let init = self.settle(chain, op.id);
            context.append_dep_operand(op.id, init);
        }
        for &chain in &carried {
            // The ports the op already carries are wired up; all that is left is
            // the result naming the state it leaves behind.
            let result = context.grow_dep_port(op.id, None, |_, _| None);
            self.chains.insert(chain, ChainState::opened(result));
        }
    }

    /// Every chain something inside `op`'s regions touches, read the way the walk
    /// of those regions will read it.
    fn chains_touched_under(&self, op: &OpHandle) -> BTreeSet<Chain> {
        op.regions()
            .iter()
            .flat_map(|&region| scopes::region_ops(self.context, region))
            .flat_map(|op_id| match classify(&self.context.get_op(op_id)) {
                Some(Kind::Read(address) | Kind::Write(address)) => vec![self.chain(address)],
                Some(Kind::Clobber | Kind::Export) => self.exposed(),
                _ => Vec::new(),
            })
            .collect()
    }

    /// The chains the outside can reach, in port order.
    fn exposed(&self) -> Vec<Chain> {
        self.chains
            .keys()
            .copied()
            .filter(|&chain| !self.objects.is_private(chain))
            .collect()
    }

    /// Whether `slot`'s allocation opens a chain of its own here.
    fn opens(&self, slot: ValueId) -> bool {
        self.objects.distinguished.contains(&Base::Alloca(slot))
    }

    /// The chain an access at `address` sits on. A chain the walk has not opened
    /// yet — a slot allocated inside a region the accesses of which are read from
    /// outside it — is no chain at all, and the access falls back on the
    /// conservative one, which every scope carries.
    fn chain(&self, address: ValueId) -> Chain {
        let chain = self.objects.chain_of(address);
        if self.chains.contains_key(&chain) {
            chain
        } else {
            Chain::Conservative
        }
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

    /// The state naming `chain`'s memory after every read of the fork open on it,
    /// which is what an operation that does not merely observe it has to take: the
    /// state the last write left where no read has forked off it, that one read's
    /// where a single one has, and their `state.join` otherwise. The join goes
    /// where the operation taking it is, so it is defined before it is read.
    fn settle(&mut self, chain: Chain, before: OpId) -> ValueId {
        let reads = std::mem::take(&mut self.chain_mut(chain).reads);
        let settled = match reads.len() {
            0 => self.chain_mut(chain).written,
            1 => reads[0],
            _ => self.join(reads, before),
        };
        self.chain_mut(chain).written = settled;
        settled
    }

    /// The one state every chain in `chains` reached, which is what an operation
    /// touching them all observes.
    fn merge(&mut self, chains: &[Chain], before: OpId) -> ValueId {
        let states = chains
            .iter()
            .map(|&chain| self.settle(chain, before))
            .collect::<Vec<_>>();
        match states.as_slice() {
            [only] => *only,
            _ => self.join(states, before),
        }
    }

    /// Name each chain again in the state an operation touching them all left, so
    /// what each carries on from is ordered after it.
    fn spread(&mut self, chains: &[Chain], state: ValueId, after: OpId) {
        if let [only] = chains {
            self.chains.insert(*only, ChainState::opened(state));
            return;
        }
        let mut op = SplitOpBuilder::new(self.context).dep_operand(state);
        for _ in chains {
            op = op.dep_result();
        }
        let op = op.build();
        let block = self.block_of(after);
        block.insert(self.position_of(after) + 1, op.id());
        for (&chain, &named) in chains.iter().zip(op.states().iter()) {
            self.chains.insert(chain, ChainState::opened(named));
        }
    }

    fn join(&mut self, states: Vec<ValueId>, before: OpId) -> ValueId {
        let mut op = JoinOpBuilder::new(self.context).dep_result();
        for state in states {
            op = op.dep_operand(state);
        }
        let op = op.build();
        self.block_of(before)
            .insert(self.position_of(before), op.id());
        op.result()
    }

    fn block_of(&self, op: OpId) -> BlockHandle {
        self.context
            .get_block(self.context.parent_block(op).expect("an op in a block"))
    }

    fn position_of(&self, op: OpId) -> usize {
        self.block_of(op)
            .op_ids()
            .iter()
            .position(|&id| id == op)
            .expect("the op is in its own block")
    }

    fn chain_mut(&mut self, chain: Chain) -> &mut ChainState {
        self.chains
            .get_mut(&chain)
            .expect("the chain reaches this point")
    }
}

/// The objects that get a chain of their own.
///
/// An object qualifies when the facts tell it apart from every other object the
/// function's accesses name, and when no access reads an address they cannot
/// resolve — such an address may be any object they cannot rule it out of, which
/// leaves only the private slots no address can reach.
///
/// A chain the outside can reach pays for itself only where another such chain
/// carries accesses too: an object that is the whole of exposed memory as far as
/// the order goes is named twice for nothing — once as itself, once as the
/// conservative chain a call and a return still name — and every call in between
/// spends a join and a split putting the two back together.
fn distinguished(context: &Context, facts: &AliasFacts, ops: &[OpId]) -> BTreeSet<Base> {
    let mut named = BTreeSet::new();
    let mut opaque = false;
    for &op_id in ops {
        for address in accessed_addresses(&context.get_op(op_id)) {
            match facts.fact(address) {
                PointerFact::Object { base, .. } => {
                    named.insert(base);
                }
                _ => opaque = true,
            }
        }
    }
    let told_apart = named
        .iter()
        .copied()
        .filter(|base| {
            named
                .iter()
                .all(|other| other == base || base.distinct(*other))
        })
        .filter(|&base| !opaque || facts.is_private(base))
        .collect::<BTreeSet<_>>();

    let exposed = told_apart
        .iter()
        .filter(|&&base| !facts.is_private(base))
        .count();
    let conservative_accessed = opaque || named.iter().any(|base| !told_apart.contains(base));
    if exposed + usize::from(conservative_accessed) > 1 {
        return told_apart;
    }
    told_apart
        .into_iter()
        .filter(|&base| facts.is_private(base))
        .collect()
}

/// The addresses `op` names an access of. A call or a `memcpy` names none: it is
/// ordered against every object the outside can reach, whichever those are.
fn accessed_addresses(op: &OpHandle) -> Vec<ValueId> {
    match classify(op) {
        Some(Kind::Read(address) | Kind::Write(address)) => vec![address],
        Some(Kind::Open(slot)) => vec![slot],
        _ => Vec::new(),
    }
}

/// What `op` does to memory.
fn classify(op: &OpHandle) -> Option<Kind> {
    if let Some(allocation) = op.clone().as_interface::<dyn PromotableAllocation>() {
        return Some(Kind::Open(allocation.promoted_location()));
    }
    // Both interfaces are asked before either answers: an operation declaring
    // the two writes the extent it reads, and is no observer.
    if let Some(write) = op.clone().as_interface::<dyn MemoryWrite>() {
        return Some(Kind::Write(write.write_location()));
    }
    if let Some(read) = op.clone().as_interface::<dyn MemoryRead>() {
        return Some(Kind::Read(read.read_location()));
    }
    if op.is::<MemcpyOp>() || op.is::<CallOp>() {
        return Some(Kind::Clobber);
    }
    op.is::<ReturnOp>().then_some(Kind::Export)
}

/// Whether `op` is one of the operations that carry state. Exporting the chains is
/// not touching memory: a `return` alone leaves a function with nothing to thread.
fn touches_memory(op: &OpHandle) -> bool {
    matches!(
        classify(op),
        Some(Kind::Open(_) | Kind::Read(_) | Kind::Write(_) | Kind::Clobber)
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

/// Whether any operation already names a dependency.
fn already_threaded(context: &Context, ops: &[OpId]) -> bool {
    ops.iter().any(|&op_id| {
        let op = context.get_op(op_id);
        !op.dep_operands().is_empty() || !op.dep_results().is_empty()
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
        .flat_map(|&region| scopes::region_ops(context, region))
        .any(|op_id| touches_memory(&context.get_op(op_id)))
}

/// Remove the memory order from `function`, leaving the program it ordered.
///
/// Answers whether anything came off; idempotent.
pub fn unthread(
    context: &Context,
    rewriter: &mut Rewriter,
    function: &OpHandle,
) -> Result<bool, PassError> {
    let Some(&body) = function.regions().first() else {
        return Ok(false);
    };
    let ops = scopes::region_ops(context, body);
    if !already_threaded(context, &ops) {
        return Ok(false);
    }
    for &op in &ops {
        context.clear_dep_operands(op);
    }
    for block in blocks_under(context, body) {
        context.clear_dep_arguments(block);
    }
    for &op in &ops {
        context.clear_dep_results(op);
    }
    for &op in &ops {
        if context.get_op(op).dialect().as_str() == "state" {
            rewriter.erase_op(&OperationRef::new(context.get_op(op)))?;
        }
    }
    Ok(true)
}

fn blocks_under(context: &Context, region: crate::RegionId) -> Vec<crate::BlockId> {
    let mut blocks = Vec::new();
    for block in context.get_region(region).iter(context.clone()) {
        blocks.push(block.id());
        for op_id in block.op_ids() {
            for nested in context.get_op(op_id).regions() {
                blocks.extend(blocks_under(context, nested));
            }
        }
    }
    blocks
}

/// [`unthread`] as a pass. No production pipeline runs it; it exists so a
/// pipeline can prove the round trip.
#[derive(Default)]
pub struct UnthreadPass;

impl UnthreadPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(UnthreadPass, "unthread");

impl Pass for UnthreadPass {
    fn name(&self) -> &'static str {
        "unthread"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Blocks)
    }

    fn run(
        &mut self,
        operation: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        unthread(context, rewriter, operation.op()).map(|_| ())
    }
}
