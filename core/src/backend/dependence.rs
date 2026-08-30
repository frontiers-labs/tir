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

use std::collections::HashMap;

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
    /// Predecessors as indices into `ops`, ascending and deduplicated.
    predecessors: Vec<Vec<usize>>,
}

impl Dependences {
    /// The graph of `ops`, taken in the reference order the slice spells.
    /// `assignment` is empty before register allocation, where a value is its
    /// own resource and the SSA edges are the whole story.
    pub fn of_ops(context: &Context, ops: &[OpId], assignment: &RegAssignment) -> Self {
        let mut graph = Self {
            ops: ops.to_vec(),
            predecessors: value_edges(context, ops),
        };
        for (entry, parameter) in graph
            .predecessors
            .iter_mut()
            .zip(parameter_edges(context, ops))
        {
            entry.extend(parameter);
        }
        graph.add_register_edges(context, assignment);
        graph.add_terminator_barriers(context);
        for entry in &mut graph.predecessors {
            entry.sort_unstable();
            entry.dedup();
        }
        graph
    }

    /// A register a definition and a use share orders them: a read follows the
    /// last write, and a write follows both the last write and every read since
    /// it.
    fn add_register_edges(&mut self, context: &Context, assignment: &RegAssignment) {
        let mut written: HashMap<Resource, usize> = HashMap::new();
        let mut read: HashMap<Resource, Vec<usize>> = HashMap::new();
        for index in 0..self.ops.len() {
            let op = context.get_op(self.ops[index]);
            let regs = execution_regs(&op);
            let placed = |value: &ValueId| assignment.get(*value).map(resource);
            let mut follows = |producer: usize| {
                if producer != index {
                    self.predecessors[index].push(producer);
                }
            };
            for register in regs
                .phys_uses
                .iter()
                .copied()
                .map(resource)
                .chain(regs.uses.iter().filter_map(placed))
            {
                if let Some(&writer) = written.get(&register) {
                    follows(writer);
                }
                read.entry(register).or_default().push(index);
            }
            for register in regs
                .phys_defs
                .iter()
                .copied()
                .map(resource)
                .chain(regs.defs.iter().filter_map(placed))
            {
                if let Some(&writer) = written.get(&register) {
                    follows(writer);
                }
                for reader in read.remove(&register).unwrap_or_default() {
                    follows(reader);
                }
                written.insert(register, index);
            }
        }
    }

    /// What leaves a block is not a scheduling question: a terminator is a
    /// barrier where it stands, by rule rather than by edge. That is last for
    /// the branch a block ends on, and the place it holds for the markers that
    /// close a symbol or a section behind it.
    fn add_terminator_barriers(&mut self, context: &Context) {
        for index in 0..self.ops.len() {
            if context
                .get_op(self.ops[index])
                .as_interface::<dyn Terminator>()
                .is_none()
            {
                continue;
            }
            self.predecessors[index].extend(0..index);
            for later in &mut self.predecessors[index + 1..] {
                later.push(index);
            }
        }
    }

    /// The operations the reference order's `index` must follow, as indices
    /// into that same order.
    pub fn predecessors(&self, index: usize) -> &[usize] {
        &self.predecessors[index]
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
        let mut successors: Vec<Vec<usize>> = vec![Vec::new(); self.ops.len()];
        let mut pending: Vec<usize> = self.predecessors.iter().map(Vec::len).collect();
        for (node, predecessors) in self.predecessors.iter().enumerate() {
            for &predecessor in predecessors {
                successors[predecessor].push(node);
            }
        }
        let mut ready: Vec<usize> = (0..self.ops.len()).filter(|&n| pending[n] == 0).collect();
        let mut order = Vec::with_capacity(self.ops.len());
        while !ready.is_empty() {
            let node = pick(&mut ready);
            order.push(self.ops[node]);
            for &next in &successors[node] {
                pending[next] -= 1;
                if pending[next] == 0 {
                    ready.push(next);
                }
            }
        }
        (order.len() == self.ops.len()).then_some(order)
    }
}

/// Each operation's definitions among `ops`, as indices into it. A duplicated
/// edge is harmless: the predecessor lists are deduplicated once the graph is
/// complete.
///
/// A block parameter is live in wherever it is named, so an operation writing
/// one does not define what the block read on entry; those are
/// [`parameter_edges`], and only they order it.
fn value_edges(context: &Context, ops: &[OpId]) -> Vec<Vec<usize>> {
    let defined: HashMap<ValueId, usize> = ops
        .iter()
        .enumerate()
        .flat_map(|(index, &op)| {
            context
                .get_op(op)
                .results()
                .into_iter()
                .map(move |result| (result, index))
        })
        .filter(|(result, _)| !context.is_block_argument(*result))
        .collect();
    let mut predecessors = vec![Vec::new(); ops.len()];
    for (index, &op) in ops.iter().enumerate() {
        for operand in subtree_operands(context, &context.get_op(op)) {
            match defined.get(&operand) {
                Some(&producer) if producer != index => predecessors[index].push(producer),
                _ => {}
            }
        }
    }
    predecessors
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

/// The order a block parameter carries where destruction left it written in the
/// block that reads it — the back edge of a self-loop assigns the parameter its
/// own block was entered with. The parameter is a register there, not a
/// definition: an operation ahead of the write reads what the block was entered
/// with, so the write follows it; one behind reads what the write left, so it
/// follows the write; and one write follows the last.
fn parameter_edges(context: &Context, ops: &[OpId]) -> Vec<Vec<usize>> {
    let mut predecessors = vec![Vec::new(); ops.len()];
    let mut written: HashMap<ValueId, usize> = HashMap::new();
    let mut read: HashMap<ValueId, Vec<usize>> = HashMap::new();
    for (index, &op_id) in ops.iter().enumerate() {
        let op = context.get_op(op_id);
        for value in subtree_operands(context, &op) {
            if !context.is_block_argument(value) {
                continue;
            }
            if let Some(&writer) = written.get(&value) {
                predecessors[index].push(writer);
            }
            read.entry(value).or_default().push(index);
        }
        for value in op.results() {
            if !context.is_block_argument(value) {
                continue;
            }
            if let Some(&writer) = written.get(&value) {
                predecessors[index].push(writer);
            }
            for reader in read.remove(&value).unwrap_or_default() {
                predecessors[index].push(reader);
            }
            written.insert(value, index);
        }
    }
    predecessors
}

/// Every value read by `op` or by anything under it: an operation holding a
/// region names the values around it, and can only run where they do.
fn subtree_operands(context: &Context, op: &OpHandle) -> Vec<ValueId> {
    let mut operands = op.operands().to_vec();
    for region in op.regions().iter().copied() {
        for block in context.get_region(region).iter(context.clone()) {
            for nested in block.op_ids() {
                operands.extend(subtree_operands(context, &context.get_op(nested)));
            }
        }
    }
    operands
}
