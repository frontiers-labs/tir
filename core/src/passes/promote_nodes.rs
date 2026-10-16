//! Demand annotation over unordered regions: a local slot's value on the ports
//! of the loops and gates its accesses cross. `docs/design/ir.md` §5.3 states
//! what is promoted and what stays a slot.
//!
//! The walk runs twice: once probing, which grows no port and rewrites nothing,
//! and again for real once the probe has shown that every state the growth
//! reads holds one value for the slot. Region membership decides nothing: two
//! accesses in one region are ordered by the chain alone, and insertion order
//! is never read.

use std::collections::{HashMap, HashSet};

use crate::analysis::slots::{SlotState, agreed_value_type, collect_slots};
use crate::analysis::{AnalysisManager, EscapeFacts};
use crate::analysis::{chain, regions};
use crate::binding::StateChain;
use crate::func::FuncOp;
use crate::{
    Context, Gamma, MemoryRead, MemoryWrite, OpHandle, OpId, OperationRef, Pass, PassError,
    PassTarget, RegionId, RegionKind, Theta, TypeId, ValueId,
};

#[derive(Clone, Default)]
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

    fn settles(&self) -> bool {
        true
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        let ops = regions::region_ops(context, body);
        let escapes = analyses.get::<EscapeFacts>(context, op.op().id);
        for (slot, state) in collect_slots(context, &escapes, &ops) {
            let Some(ty) = promotable(context, slot, &state, body) else {
                continue;
            };
            // A read the chain cannot answer, and a port the growth would
            // have no value to enter, keep the slot memory: the write either
            // would go with the promotion or was never there.
            if Promoter::new(context, slot, ty, true).refuses(&state) {
                continue;
            }
            Promoter::new(context, slot, ty, false).promote(&state)?;
        }
        Ok(())
    }
}

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
    if !address_only_accessed(context, slot, state) {
        return None;
    }
    let ty = agreed_value_type(context, state)?;
    accesses()
        .all(|op| crosses_declared_bindings(context, op, body))
        .then_some(ty)
}

fn address_only_accessed(context: &Context, address: ValueId, state: &SlotState) -> bool {
    context.users_of(address).into_iter().all(|user| {
        let instance = context.get_op(user);
        if instance.is::<crate::ptr::PtrAddOp>() {
            return instance
                .results()
                .iter()
                .all(|&derived| address_only_accessed(context, derived, state));
        }
        state.loads.contains(&user) || state.stores.contains(&user)
    })
}

fn names_whole_slot(context: &Context, op: OpId, slot: ValueId) -> bool {
    let instance = context.get_op(op);
    crate::analysis::access_of(&instance).is_some_and(|access| access.location == slot)
        && !instance.state_operands().is_empty()
}

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
#[derive(Clone, Copy, PartialEq)]
enum Reach {
    Value(ValueId),
    /// The value growing the port a loop or a gate carries on this chain will
    /// put here. It is a value of its own, so it is the same answer as itself
    /// and a different one from every value already in the IR.
    Written(ValueId),
    /// Nothing wrote the slot on the way here.
    Undefined,
    /// Two memories merged here left the slot holding different values, so no
    /// one value stands at this point.
    Unknown,
}

impl Reach {
    fn merge(self, other: Reach) -> Reach {
        match (self, other) {
            (Reach::Undefined, found) | (found, Reach::Undefined) => found,
            (a, b) if a == b => a,
            _ => Reach::Unknown,
        }
    }
}

struct Promoter<'a> {
    context: &'a Context,
    slot: ValueId,
    ty: TypeId,
    /// The slot's value at each dependency already walked.
    reach: HashMap<ValueId, Reach>,
    /// The chains, by the state their op leaves, whose port for the slot has
    /// been grown.
    grown: HashSet<ValueId>,
    /// Whether a read of what nothing wrote stands, keeping the allocation.
    kept: bool,
    /// What this sweep has already handed each retired value on to.
    substituted: HashMap<ValueId, ValueId>,
    /// Whether this walk is only asking whether the growth would stand: it
    /// grows no port and rewrites nothing, and records what a grown port would
    /// carry instead.
    probing: bool,
    /// Set where a state the growth reads holds no one value for the slot.
    refused: bool,
}

impl<'a> Promoter<'a> {
    fn new(context: &'a Context, slot: ValueId, ty: TypeId, probing: bool) -> Self {
        Self {
            context,
            slot,
            ty,
            reach: HashMap::new(),
            grown: HashSet::new(),
            kept: false,
            substituted: HashMap::new(),
            probing,
            refused: false,
        }
    }

    fn refuses(&mut self, state: &SlotState) -> bool {
        for &load in &state.loads {
            let Some(&observed) = self.context.get_op(load).state_operands().first() else {
                return true;
            };
            if self.reach(observed) == Reach::Unknown {
                return true;
            }
        }
        self.refused
    }

    fn promote(&mut self, state: &SlotState) -> Result<(), PassError> {
        let context = self.context;
        let reached: Vec<Reach> = state
            .loads
            .iter()
            .map(|&load| self.reach(context.get_op(load).state_operands()[0]))
            .collect();
        // A read the chain cannot answer keeps the slot memory: a write it may
        // have observed would go with the promotion.
        if reached.contains(&Reach::Unknown) {
            return Ok(());
        }
        let mut dead = state.stores.clone();
        for (&load, &found) in state.loads.iter().zip(&reached) {
            let instance = context.get_op(load);
            match found {
                Reach::Value(value) => {
                    let read = instance
                        .clone()
                        .as_interface::<dyn MemoryRead>()
                        .expect("a collected load reads")
                        .read_value();
                    let value = self.retired(value);
                    self.replace(load, read, value);
                    self.substituted.insert(read, value);
                    dead.push(load);
                }
                _ => self.kept = true,
            }
        }
        for &op in &dead {
            let instance = context.get_op(op);
            let observed = self.retired(instance.state_operands()[0]);
            for left in instance.state_results() {
                self.replace(op, left, observed);
                self.substituted.insert(left, observed);
            }
        }
        let alloca = state.alloca.filter(|_| !self.kept);
        for op in dead.into_iter().chain(alloca) {
            context.erase_op(&OperationRef::new(context.get_op(op)))?;
        }
        Ok(())
    }

    fn retired(&self, value: ValueId) -> ValueId {
        let mut current = value;
        while let Some(&next) = self.substituted.get(&current) {
            if next == current {
                break;
            }
            current = next;
        }
        current
    }

    fn replace(&self, op: OpId, old: ValueId, new: ValueId) {
        let context = self.context;
        context.replace_value_uses(old, new);
        if let Some(region) = context.parent_nodes_region(op) {
            context.rename_region_results(region, old, new, &[]);
        }
    }

    fn reach(&mut self, dep: ValueId) -> Reach {
        if let Some(&known) = self.reach.get(&dep) {
            return known;
        }
        let context = self.context;
        let found = match context.get_value(dep).defining_op() {
            None => self.crossing(dep),
            Some(def) => {
                let instance = context.get_op(def);
                if let Some(written) = self.writes(&instance) {
                    Reach::Value(written)
                } else if instance.is::<crate::state::EntryStateOp>() {
                    Reach::Undefined
                } else if instance.is::<crate::state::JoinOp>() {
                    instance
                        .state_operands()
                        .iter()
                        .fold(Reach::Undefined, |found, &state| {
                            found.merge(self.reach(state))
                        })
                } else if instance.regions().is_empty() {
                    // An effect the walk cannot read still names the memory
                    // before it: the seam an inlined body leaves sits on the
                    // chain, and stepping over it would hide the write it holds.
                    self.reach(instance.state_operands()[0])
                } else {
                    self.crossing(dep)
                }
            }
        };
        self.reach.insert(dep, found);
        found
    }

    fn crossing(&mut self, dep: ValueId) -> Reach {
        let chain::Step::Port {
            op,
            chain,
            entering,
        } = chain::back(self.context, dep)
        else {
            return Reach::Undefined;
        };
        let op = self.context.get_op(op);
        let repeats = op.has_interface::<dyn Theta>();
        if !self.writes_under(&op) || (entering && !repeats) {
            return self.reach(chain.entered);
        }
        if repeats {
            self.grow_theta(&op, &chain);
        } else {
            self.grow_gamma(&op, &chain);
        }
        self.reach[&dep]
    }

    fn grow_theta(&mut self, op: &OpHandle, chain: &StateChain) {
        if self.grown.contains(&chain.left) {
            return;
        }
        let context = self.context;
        let theta = op.clone().as_interface::<dyn Theta>().expect("a loop");
        let body = theta.body();
        let entered = chain.entered;
        // Spelling the init may grow an enclosing loop, whose latch walks back
        // into this one and grows it on the way: mark it grown only after.
        let init = self.reach(entered);
        if !self.grown.insert(chain.left) {
            return;
        }
        let region = context.get_region(body);
        let (continue_dep, exit_dep) = (
            chain.next.expect("a loop carries a state on"),
            chain.exits[0],
        );
        let port_dep = chain.ports[0];
        let left = chain.left;
        if self.probing {
            let grown = Reach::Written(left);
            self.reach.insert(port_dep, grown);
            self.reach.insert(left, grown);
            for state in [entered, continue_dep, exit_dep] {
                self.demand(state);
            }
            return;
        }
        let init = self.value_of(init);
        let mut grown = None;
        let result = context.grow_port(op.id, self.ty, Some(init), |_, port| {
            let port = port.expect("a loop port is entered on a value");
            self.reach.insert(port_dep, Reach::Value(port));
            let carried = self.held(continue_dep);
            grown = Some((port, self.held(exit_dep)));
            Some(carried)
        });
        let (port, exit) = grown.expect("the latch ran");
        // The grown port leaves the loop as itself until the exit cone says
        // what the slot holds there.
        let binding = op
            .clone()
            .as_interface::<dyn Theta>()
            .expect("a loop")
            .binding();
        let mut results = region.results();
        let at = binding
            .exit
            .clone()
            .find(|&index| results[index] == port)
            .expect("the grown port is its own exit value");
        if results[at] != exit {
            results[at] = exit;
            context.set_region_results(body, results);
        }
        self.reach.insert(left, Reach::Value(result));
    }

    fn grow_gamma(&mut self, op: &OpHandle, chain: &StateChain) {
        if self.grown.contains(&chain.left) {
            return;
        }
        // Spelling the state the gate is entered on may grow an enclosing
        // loop, whose latch walks back into this gate and grows it on the
        // way: mark it grown only after.
        let context = self.context;
        self.reach(chain.entered);
        if !self.grown.insert(chain.left) {
            return;
        }
        let left = chain.left;
        let arm_left = |arm| {
            let at = op.regions().iter().position(|&region| region == arm);
            chain.exits[at.expect("an arm of the gate")]
        };
        if self.probing {
            self.reach.insert(left, Reach::Written(left));
            for arm in op.regions() {
                self.demand(arm_left(arm));
            }
            return;
        }
        let result = context.grow_port(op.id, self.ty, None, |arm, _| {
            Some(self.held(arm_left(arm)))
        });
        self.reach.insert(left, Reach::Value(result));
    }

    fn held(&mut self, dep: ValueId) -> ValueId {
        let found = self.reach(dep);
        self.value_of(found)
    }

    fn value_of(&self, found: Reach) -> ValueId {
        match found {
            Reach::Value(value) => value,
            _ => unreachable!("a port is entered on a written slot"),
        }
    }

    fn demand(&mut self, state: ValueId) {
        if !matches!(self.reach(state), Reach::Value(_) | Reach::Written(..)) {
            self.refused = true;
        }
    }

    fn writes(&self, op: &OpHandle) -> Option<ValueId> {
        let write = op.clone().as_interface::<dyn MemoryWrite>()?;
        (write.write_location() == self.slot).then(|| write.written_value())
    }

    fn writes_under(&self, op: &OpHandle) -> bool {
        regions::subtree_ops(self.context, op)
            .into_iter()
            .any(|inner| self.writes(&self.context.get_op(inner)).is_some())
    }
}
