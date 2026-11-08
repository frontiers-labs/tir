//! The dependence DAG of a machine block.
//!
//! A block's order is one linearization of this graph and nothing more:
//! selection emits one, the verifier accepts any, and a scheduler may choose
//! another. The edges are the ones the machine form already carries —
//!
//! - **values**: an SSA operand follows the operation defining it, memory state
//!   included, since a state port is an operand like any other. A block
//!   parameter is the exception destruction leaves behind: it is live in, so an
//!   operation writing one — the back edge of a self-loop — is a register write
//!   over the block that reads it, ordered like the resources below;
//! - **register resources**: the registers an instruction names without a
//!   value — a physical literal in a slot, the fixed registers its behavior
//!   touches (x86 `EFLAGS` between a compare and the branch reading it), the
//!   set a call destroys — read and written over one register file's index.
//!   Where a [`RegAssignment`] places values in registers, those are resources
//!   too, and the graph gains the anti- and output edges a post-allocation
//!   order must respect.
//!
//! A value edge is a fact. An anti- or output edge is not: it preserves an
//! order, and which order it preserves is the one the builder is handed — the
//! block's own for the verifier and the shuffler, the order selection meant for
//! emission. So the caller owes the builder an order the value edges already
//! admit: a reference that reads a definition before it is made would bind a
//! flag reader to the wrong writer, and preserving it would preserve nothing.
//! That is the rule [`verify_block_order`] checks, and the one selection's
//! cover order satisfies by construction.

use tir::{BlockHandle, Context, Error, OpHandle, OpId, Terminator, ValueId, utils::Rng};

use crate::analysis::defuse::{PhysReg, execution_regs};
use crate::backend::RegAssignment;

/// A register resource: the physical register file and the index in it. Two
/// classes over one file at one index — `GPR`, `GPR32`, `GPR8` — name the same
/// register, so the class a slot happens to be typed through is not part of the
/// identity. A narrower architectural view of the same index (an x86 high byte)
/// counts as the whole register: conservative, and the only cost is an order no
/// scheduler was going to need.
type Resource = (&'static str, u16);

fn resource((class, index): PhysReg) -> Resource {
    (class.file(), index)
}

/// The operations of one block and, for each, the ones it must follow.
pub struct Dependences {
    ops: Vec<OpId>,
    /// Predecessors as indices into `ops`, ascending and deduplicated: those
    /// of operation `index` are `predecessors[starts[index]..starts[index + 1]]`.
    predecessors: Vec<usize>,
    starts: Vec<usize>,
    register_users: Vec<Vec<OpId>>,
    register_live_out: Vec<bool>,
}

impl Dependences {
    /// The graph of `ops`, taken in the reference order the slice spells.
    /// `assignment` is empty before register allocation, where a value is its
    /// own resource and the SSA edges are the whole story.
    pub fn of_ops(context: &Context, ops: &[OpId], assignment: &RegAssignment) -> Self {
        let mut graph = Self {
            ops: ops.to_vec(),
            predecessors: Vec::new(),
            starts: Vec::with_capacity(ops.len() + 1),
            register_users: vec![Vec::new(); ops.len()],
            register_live_out: vec![false; ops.len()],
        };
        // `(operation, predecessor)`: the one column the graph is sorted from.
        let mut edges = Vec::new();
        value_edges(context, ops, &mut edges);
        graph.add_register_edges(context, assignment, &mut edges);
        terminator_barriers(context, ops, &mut edges);
        edges.sort_unstable();
        edges.dedup();
        graph.predecessors = edges.iter().map(|&(_, predecessor)| predecessor).collect();
        let starts = (0..=ops.len()).map(|index| edges.partition_point(|edge| edge.0 < index));
        graph.starts.extend(starts);
        graph
    }

    // A register a definition and a use share orders them; see
    // [`in_place_edges`].
    fn add_register_edges(
        &mut self,
        context: &Context,
        assignment: &RegAssignment,
        edges: &mut Vec<(usize, usize)>,
    ) {
        let mut accesses: Vec<(Resource, usize, bool)> = Vec::new();
        for (index, &op) in self.ops.iter().enumerate() {
            let regs = execution_regs(&context.get_op(op));
            let placed = |value: &ValueId| assignment.get(*value).map(resource);
            let uses = regs.phys_uses.iter().copied().map(resource);
            let uses = uses.chain(regs.uses.iter().filter_map(placed));
            accesses.extend(uses.map(|register| (register, index, false)));
            let defs = regs.phys_defs.iter().copied().map(resource);
            let defs = defs.chain(regs.defs.iter().filter_map(placed));
            accesses.extend(defs.map(|register| (register, index, true)));
        }
        let mut users: Vec<(usize, usize)> = Vec::new();
        let live_out = &mut self.register_live_out;
        in_place_edges(
            accesses,
            edges,
            |writer, reader| users.push((writer, reader)),
            |writer| live_out[writer] = true,
        );
        users.sort_unstable();
        for (writer, reader) in users {
            self.register_users[writer].push(self.ops[reader]);
        }
    }

    /// Readers of this operation's register definitions, unless a definition
    /// leaves the block. A final write remains observable in successor blocks.
    pub fn local_register_users(&self, index: usize) -> Option<&[OpId]> {
        (!self.register_live_out[index]).then_some(&self.register_users[index])
    }

    /// The operations the reference order's `index` must follow, as indices
    /// into that same order.
    pub fn predecessors(&self, index: usize) -> &[usize] {
        &self.predecessors[self.starts[index]..self.starts[index + 1]]
    }

    /// A topological order, ties broken by the reference order — so a graph the
    /// reference order already satisfies is reproduced exactly. `None` if the
    /// edges hold a cycle.
    pub fn linearize(&self) -> Option<Vec<OpId>> {
        self.order(|ready| {
            let at = (0..ready.len()).min_by_key(|&at| ready[at]).unwrap_or(0);
            ready.swap_remove(at)
        })
    }

    /// A seeded random topological order: the same edges, another linearization.
    pub fn shuffle(&self, seed: u64) -> Option<Vec<OpId>> {
        let mut rng = Rng::new(seed);
        self.order(|ready| ready.swap_remove(rng.below(ready.len())))
    }

    fn order(&self, mut pick: impl FnMut(&mut Vec<usize>) -> usize) -> Option<Vec<OpId>> {
        let count = self.ops.len();
        let mut pending: Vec<usize> = (0..count).map(|n| self.predecessors(n).len()).collect();
        // The same column by predecessor: the successors of `node`, ascending,
        // are `successors[ends[node] - its count..ends[node]]` once filled.
        let mut ends = vec![0; count + 1];
        for &predecessor in &self.predecessors {
            ends[predecessor + 1] += 1;
        }
        for node in 0..count {
            ends[node + 1] += ends[node];
        }
        let mut filled = ends.clone();
        let mut successors = vec![0; self.predecessors.len()];
        for node in 0..count {
            for &predecessor in self.predecessors(node) {
                successors[filled[predecessor]] = node;
                filled[predecessor] += 1;
            }
        }
        let mut ready: Vec<usize> = (0..count).filter(|&n| pending[n] == 0).collect();
        let mut order = Vec::with_capacity(count);
        while !ready.is_empty() {
            let node = pick(&mut ready);
            order.push(self.ops[node]);
            for &next in &successors[ends[node]..ends[node + 1]] {
                pending[next] -= 1;
                if pending[next] == 0 {
                    ready.push(next);
                }
            }
        }
        (order.len() == count).then_some(order)
    }
}

// What leaves a block is not a scheduling question: a terminator is a
// barrier where it stands, by rule rather than by edge. That is last for
// the branch a block ends on, and the place it holds for the markers that
// close a symbol or a section behind it.
fn terminator_barriers(context: &Context, ops: &[OpId], edges: &mut Vec<(usize, usize)>) {
    for (index, &op) in ops.iter().enumerate() {
        let op = context.get_op(op);
        if op.as_interface::<dyn Terminator>().is_none() {
            continue;
        }
        edges.extend((0..index).map(|earlier| (index, earlier)));
        edges.extend((index + 1..ops.len()).map(|later| (later, index)));
    }
}

// The order a name written in place carries, as edges: a read follows the
// last write, and a write follows both the last write and every read since
// it. `accesses` is `(name, operation, writes)`, in any order: an operation
// reads before it writes. `read` is told the writer and the reader of each
// read of a name written in the block, and `left` the last writer of each
// name.
fn in_place_edges<N: Ord + Copy>(
    mut accesses: Vec<(N, usize, bool)>,
    edges: &mut Vec<(usize, usize)>,
    mut read: impl FnMut(usize, usize),
    mut left: impl FnMut(usize),
) {
    accesses.sort_unstable();
    for name in accesses.chunk_by(|a, b| a.0 == b.0) {
        let mut writer = None;
        let mut unwritten = 0;
        for (at, &(_, index, writes)) in name.iter().enumerate() {
            if let Some(writer) = writer.filter(|&writer| writer != index) {
                edges.push((index, writer));
            }
            if !writes {
                if let Some(writer) = writer {
                    read(writer, index);
                }
                continue;
            }
            let readers = name[unwritten..at].iter().map(|reader| reader.1);
            edges.extend(
                readers
                    .filter(|&reader| reader != index)
                    .map(|reader| (index, reader)),
            );
            writer = Some(index);
            unwritten = at + 1;
        }
        if let Some(writer) = writer {
            left(writer);
        }
    }
}

// Each operation's definitions among `ops`, and the order the block's
// parameters carry, as edges.
//
// A block parameter is live in wherever it is named, so an operation writing
// one does not define what the block read on entry. Destruction leaves it
// written in the block that reads it: the back edge of a self-loop assigns
// the parameter its own block was entered with. The parameter is a register
// there, not a definition, ordered by [`in_place_edges`].
fn value_edges(context: &Context, ops: &[OpId], edges: &mut Vec<(usize, usize)>) {
    let mut read: Vec<(ValueId, usize)> = Vec::new();
    let mut defined: Vec<(ValueId, usize)> = Vec::new();
    let mut parameters: Vec<(ValueId, usize, bool)> = Vec::new();
    let mut holders = Vec::new();
    for (index, &op) in ops.iter().enumerate() {
        let op = context.get_op(op);
        read.extend(op.operands().iter().map(|&operand| (operand, index)));
        for result in op.results() {
            match context.block_of_argument(result) {
                Some(_) => parameters.push((result, index, true)),
                None => defined.push((result, index)),
            }
        }
        if !op.regions().is_empty() {
            holders.push(index);
        }
    }
    for index in holders {
        let nested = nested_operands(context, &context.get_op(ops[index]));
        read.extend(nested.into_iter().map(|operand| (operand, index)));
    }
    defined.sort_unstable();
    for &(value, index) in &read {
        if context.block_of_argument(value).is_some() {
            parameters.push((value, index, false));
            continue;
        }
        let after = defined.partition_point(|&(known, _)| known <= value);
        match after.checked_sub(1).map(|last| defined[last]) {
            Some((known, producer)) if known == value && producer != index => {
                edges.push((index, producer))
            }
            _ => {}
        }
    }
    in_place_edges(parameters, edges, |_, _| {}, |_| {});
}

/// Every operation of `block` follows what it depends on: the definitions it
/// reads, the memory state it observes, and the terminator follows everything
/// ahead of it.
///
/// No pass rewrites an operand to a definition further down the block on
/// purpose; one that does leaves a program whose only symptom is wrong
/// interference in a backward liveness scan, so the rule is checked rather than
/// assumed. Register resources cannot fail here — their edges are read off an
/// order this one already admits — but they are built all the same, because
/// this is the graph emission and the shuffler linearize.
pub fn verify_block_order(context: &Context, block: &BlockHandle) -> Result<(), Error> {
    let ops = block.op_ids();
    let graph = Dependences::of_ops(context, &ops, &RegAssignment::default());
    for (index, &op_id) in ops.iter().enumerate() {
        let Some(&producer) = graph
            .predecessors(index)
            .iter()
            .find(|&&producer| producer > index)
        else {
            continue;
        };
        let op = context.get_op(op_id);
        let operands = op.operands();
        let value = context
            .get_op(ops[producer])
            .results()
            .into_iter()
            .find(|result| operands.contains(result));
        return Err(Error::VerificationError(match value {
            Some(value) => format!(
                "{} reads %{}, defined later in the same block",
                op.name().as_str(),
                value.number(),
            ),
            None => format!(
                "{} precedes {}, which it depends on",
                op.name().as_str(),
                context.get_op(ops[producer]).name().as_str(),
            ),
        }));
    }
    Ok(())
}

// Every value read by anything under `op`: an operation holding a region
// names the values around it, and can only run where they do.
fn nested_operands(context: &Context, op: &OpHandle) -> Vec<ValueId> {
    let mut operands = Vec::new();
    for region in op.regions().iter().copied() {
        for block in context.get_region(region).iter(context.clone()) {
            for nested in block.op_ids() {
                let nested = context.get_op(nested);
                operands.extend(nested.operands());
                operands.extend(nested_operands(context, &nested));
            }
        }
    }
    operands
}
