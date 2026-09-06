//! Demand annotation over unordered regions: a local slot's value on the
//! ports of the loops and gates its accesses cross.
//!
//! The converter left every access on the memory chain, so the chain says
//! which write a read sees: walk the dependency a read observes back to the
//! nearest write of the slot. Where that walk leaves a loop body through its
//! dependency port, the value crosses an iteration boundary and the loop
//! carries it as a port of its own; where it leaves a gate through the gate's
//! dependency result, each arm produces the value it leaves the slot holding
//! and the gate joins them. Region membership decides nothing: two accesses in
//! one region are ordered by the chain alone, and insertion order is never read.
//!
//! What stays a slot is what [`promote`](super::promote) leaves too — an
//! escaping address, a partial access, disagreeing types — and, here, an access
//! off the chain, which nothing can order.

use std::collections::{HashMap, HashSet};

use crate::analysis::scopes;
use crate::analysis::slots::{SlotState, agreed_value_type, collect_slots};
use crate::analysis::{AnalysisManager, EscapeFacts};
use crate::func::FuncOp;
use crate::{
    Context, Gamma, MemoryRead, MemoryWrite, OpHandle, OpId, OperationRef, Pass, PassError,
    PassTarget, RegionId, RegionKind, Rewriter, Theta, TypeId, ValueId,
};

#[derive(Default)]
pub struct PromoteNodesPass;

impl PromoteNodesPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(PromoteNodesPass, "promote-nodes");

impl Pass for PromoteNodesPass {
    fn name(&self) -> &'static str {
        "promote-nodes"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Nodes)
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        let ops = scopes::region_ops(context, body);
        let escapes = analyses.get::<EscapeFacts>(context, op.op().id);
        for (slot, state) in collect_slots(context, &escapes, &ops) {
            let Some(ty) = promotable(context, slot, &state, body) else {
                continue;
            };
            let mut promoter = Promoter {
                context,
                slot,
                ty,
                reach: HashMap::new(),
                grown: HashSet::new(),
                kept: false,
            };
            promoter.promote(&state, rewriter)?;
        }
        Ok(())
    }
}

/// The type a slot's value takes once promoted, or `None` where it stays memory:
/// allocated in the body itself, never escaping, named whole by every access,
/// agreed on one type, ordered by the chain at every access, crossed only by
/// ops whose ports can be grown, and written before any loop or gate that
/// writes it is entered — a port carries a value, and the slot has none until
/// something writes it. A read of what nothing wrote is the reader's own affair
/// and stays a read; a port entered on it would be a read the program never
/// made.
fn promotable(
    context: &Context,
    slot: ValueId,
    state: &SlotState,
    body: RegionId,
) -> Option<TypeId> {
    let alloca = state.alloca?;
    if state.escapes || context.parent_nodes_region(alloca) != Some(body) {
        return None;
    }
    let accesses = || state.loads.iter().chain(&state.stores).copied();
    if accesses().any(|op| !names_whole_slot(context, op, slot)) {
        return None;
    }
    let ty = agreed_value_type(context, state)?;
    if !accesses().all(|op| crosses_declared_bindings(context, op, body)) {
        return None;
    }
    let writers: Vec<OpId> = state
        .stores
        .iter()
        .flat_map(|&store| enclosing_ops(context, store, body))
        .collect();
    writers
        .iter()
        .all(|&op| {
            context
                .get_op(op)
                .dep_operands()
                .iter()
                .all(|&dep| written_before(context, slot, dep, &writers))
        })
        .then_some(ty)
}

/// The loops and gates between `op` and `body`, innermost first.
fn enclosing_ops(context: &Context, op: OpId, body: RegionId) -> Vec<OpId> {
    let mut ops = Vec::new();
    let mut region = context.parent_nodes_region(op);
    while let Some(current) = region.filter(|&current| current != body) {
        let Some(owner) = context.get_region(current).parent_op() else {
            break;
        };
        ops.push(owner);
        region = context.parent_nodes_region(owner);
    }
    ops
}

/// Whether every path the chain took to `dep` wrote `slot`: a write, a loop or
/// gate that writes it, or the port of one, counts; the entry state does not.
fn written_before(context: &Context, slot: ValueId, dep: ValueId, writers: &[OpId]) -> bool {
    let Some(def) = context.get_value(dep).defining_op() else {
        let Some(owner) = context
            .region_of_port(dep)
            .and_then(|region| context.get_region(region).parent_op())
        else {
            return false;
        };
        if writers.contains(&owner) && context.get_op(owner).has_interface::<dyn Theta>() {
            return true;
        }
        let ports: Vec<ValueId> = context
            .region_of_port(dep)
            .map(|region| context.get_region(region).dep_arguments())
            .unwrap_or_default()
            .iter()
            .map(crate::Value::id)
            .collect();
        let operands = context.get_op(owner).dep_operands();
        return match operands.get(dep_index(&ports, dep)) {
            Some(&entered) => written_before(context, slot, entered, writers),
            None => false,
        };
    };
    let instance = context.get_op(def);
    if writers.contains(&def) {
        return true;
    }
    if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>()
        && write.write_location() == slot
    {
        return true;
    }
    if instance.is::<crate::state::EntryStateOp>() {
        return false;
    }
    if instance.regions().is_empty() {
        return written_before(context, slot, instance.dep_operands()[0], writers);
    }
    let index = dep_index(&instance.dep_results(), dep);
    written_before(context, slot, instance.dep_operands()[index], writers)
}

/// Whether `op` accesses `slot` at the slot's own address and observes a
/// dependency: the chain is what orders it against the writes it may see.
fn names_whole_slot(context: &Context, op: OpId, slot: ValueId) -> bool {
    let instance = context.get_op(op);
    let location = if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() {
        read.read_location()
    } else if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>() {
        write.write_location()
    } else {
        return false;
    };
    location == slot && !instance.dep_operands().is_empty()
}

/// Whether every op between `op` and `body` is a loop or a gate with a declared
/// binding, so a port can be grown where the slot's value crosses it.
fn crosses_declared_bindings(context: &Context, op: OpId, body: RegionId) -> bool {
    let mut region = context.parent_nodes_region(op);
    while let Some(current) = region {
        if current == body {
            return true;
        }
        let Some(owner) = context.get_region(current).parent_op() else {
            return false;
        };
        let owner = context.get_op(owner);
        if !(owner.has_interface::<dyn Theta>() || owner.has_interface::<dyn Gamma>()) {
            return false;
        }
        region = context.parent_nodes_region(owner.id);
    }
    false
}

/// The slot's value at one point of the chain.
#[derive(Clone, Copy)]
enum Reach {
    Value(ValueId),
    /// Nothing wrote the slot on the way here.
    Undefined,
}

struct Promoter<'a> {
    context: &'a Context,
    slot: ValueId,
    ty: TypeId,
    /// The slot's value at each dependency already walked.
    reach: HashMap<ValueId, Reach>,
    /// The ops whose port for the slot has been grown.
    grown: HashSet<(OpId, usize)>,
    /// Whether a read of what nothing wrote stands, keeping the allocation.
    kept: bool,
}

impl Promoter<'_> {
    fn promote(&mut self, state: &SlotState, rewriter: &mut Rewriter) -> Result<(), PassError> {
        let context = self.context;
        let mut dead = state.stores.clone();
        for &load in &state.loads {
            let instance = context.get_op(load);
            let observed = instance.dep_operands()[0];
            match self.reach(observed) {
                Reach::Value(value) => {
                    let read = instance
                        .clone()
                        .as_interface::<dyn MemoryRead>()
                        .expect("a collected load reads")
                        .read_value();
                    self.replace(load, read, value);
                    dead.push(load);
                }
                Reach::Undefined => self.kept = true,
            }
        }
        for &op in &dead {
            let instance = context.get_op(op);
            let observed = instance.dep_operands()[0];
            for left in instance.dep_results() {
                self.replace(op, left, observed);
            }
        }
        let alloca = state.alloca.filter(|_| !self.kept);
        for op in dead.into_iter().chain(alloca) {
            rewriter.erase_op(&OperationRef::new(context.get_op(op)))?;
        }
        Ok(())
    }

    /// Hand every reader of `old`, a value `op` defines, `new` instead —
    /// region result lists included, which no use list reaches.
    fn replace(&self, op: OpId, old: ValueId, new: ValueId) {
        let context = self.context;
        context.replace_value_uses(old, new);
        if let Some(region) = context.parent_nodes_region(op) {
            context.rename_region_results(region, old, new, &[]);
        }
    }

    /// The slot's value where the chain stands at `dep`: walk back to the write
    /// that put it there, growing a port wherever the walk crosses a loop or a
    /// gate that writes the slot.
    fn reach(&mut self, dep: ValueId) -> Reach {
        if let Some(&known) = self.reach.get(&dep) {
            return known;
        }
        let context = self.context;
        let found = match context.get_value(dep).defining_op() {
            None => self.reach_port(dep),
            Some(def) => {
                let instance = context.get_op(def);
                if let Some(written) = self.writes(&instance) {
                    Reach::Value(written)
                } else if instance.is::<crate::state::EntryStateOp>() {
                    Reach::Undefined
                } else if instance.regions().is_empty() {
                    self.reach(instance.dep_operands()[0])
                } else {
                    self.reach_result(&instance, dep)
                }
            }
        };
        self.reach.insert(dep, found);
        found
    }

    /// The slot's value on entry to the region whose dependency port `dep` is.
    fn reach_port(&mut self, dep: ValueId) -> Reach {
        let context = self.context;
        let Some(region) = context.region_of_port(dep) else {
            return Reach::Undefined;
        };
        let handle = context.get_region(region);
        let ports: Vec<ValueId> = handle
            .dep_arguments()
            .iter()
            .map(crate::Value::id)
            .collect();
        let index = dep_index(&ports, dep);
        let Some(owner) = handle.parent_op() else {
            return Reach::Undefined;
        };
        let owner = context.get_op(owner);
        if owner.has_interface::<dyn Gamma>() || !self.writes_under(&owner) {
            return self.reach(owner.dep_operands()[index]);
        }
        if !owner.has_interface::<dyn Theta>() {
            return Reach::Undefined;
        }
        self.grow_theta(&owner, index);
        self.reach[&dep]
    }

    /// The slot's value after `op`, a loop or a gate, along its dependency
    /// result `dep`.
    fn reach_result(&mut self, op: &OpHandle, dep: ValueId) -> Reach {
        let index = dep_index(&op.dep_results(), dep);
        if !self.writes_under(op) {
            return self.reach(op.dep_operands()[index]);
        }
        if op.has_interface::<dyn Theta>() {
            self.grow_theta(op, index);
        } else {
            self.grow_gamma(op, index);
        }
        self.reach[&dep]
    }

    /// A loop's port for the slot: entered on the value before the loop, the
    /// body reads the port and the value it leaves the slot holding along the
    /// continue dependency is what the next iteration carries; the value along
    /// the exit dependency is what the loop produces. Both are recorded on the
    /// loop's dependency port and result at `index`.
    fn grow_theta(&mut self, op: &OpHandle, index: usize) {
        if self.grown.contains(&(op.id, index)) {
            return;
        }
        let context = self.context;
        let theta = op.clone().as_interface::<dyn Theta>().expect("a loop");
        let body = theta.body();
        let entered = op.dep_operands()[index];
        // Spelling the init may grow an enclosing loop, whose latch walks back
        // into this one and grows it on the way: mark it grown only after.
        let init = self.held(entered);
        if !self.grown.insert((op.id, index)) {
            return;
        }
        let region = context.get_region(body);
        let dep_results = region.dep_results();
        let (continue_dep, exit_dep) = (
            dep_results[index],
            dep_results[dep_results.len() / 2 + index],
        );
        let port_dep = region.dep_arguments()[index].id();
        let mut exit = None;
        let result = context.grow_port(op.id, self.ty, Some(init), |_, port| {
            let port = port.expect("a loop port is entered on a value");
            self.reach.insert(port_dep, Reach::Value(port));
            let carried = self.held(continue_dep);
            exit = Some(self.held(exit_dep));
            Some(carried)
        });
        let exit = exit.expect("the latch ran");
        let binding = op
            .clone()
            .as_interface::<dyn Theta>()
            .expect("a loop")
            .carried();
        let mut results = region.results();
        let slot = binding.exit.end - 1;
        if results[slot] != exit {
            results[slot] = exit;
            context.set_region_results(body, results, region.dep_results().len());
        }
        self.reach
            .insert(op.dep_results()[index], Reach::Value(result));
    }

    /// A gate's port for the slot: every arm produces the value it leaves the
    /// slot holding along its dependency result, and the gate's result is the
    /// value after it.
    fn grow_gamma(&mut self, op: &OpHandle, index: usize) {
        if self.grown.contains(&(op.id, index)) {
            return;
        }
        // Spelling the state the gate is entered on may grow an enclosing
        // loop, whose latch walks back into this gate and grows it on the
        // way: mark it grown only after.
        self.reach(op.dep_operands()[index]);
        if !self.grown.insert((op.id, index)) {
            return;
        }
        let context = self.context;
        let result = context.grow_port(op.id, self.ty, None, |arm, _| {
            let left = context.get_region(arm).dep_results()[index];
            Some(self.held(left))
        });
        self.reach
            .insert(op.dep_results()[index], Reach::Value(result));
    }

    /// The value the slot holds at `dep`, which [`promotable`] proved a write
    /// put there.
    fn held(&mut self, dep: ValueId) -> ValueId {
        match self.reach(dep) {
            Reach::Value(value) => value,
            Reach::Undefined => unreachable!("a port is entered on a written slot"),
        }
    }

    /// The value a write to the slot leaves it holding.
    fn writes(&self, op: &OpHandle) -> Option<ValueId> {
        let write = op.clone().as_interface::<dyn MemoryWrite>()?;
        (write.write_location() == self.slot).then(|| write.written_value())
    }

    /// Whether anything in `op`'s region tree writes the slot.
    fn writes_under(&self, op: &OpHandle) -> bool {
        scopes::subtree_ops(self.context, op)
            .into_iter()
            .any(|inner| self.writes(&self.context.get_op(inner)).is_some())
    }
}

fn dep_index(deps: &[ValueId], dep: ValueId) -> usize {
    deps.iter()
        .position(|held| *held == dep)
        .expect("a dependency of the region or op it was read off")
}
