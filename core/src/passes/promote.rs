//! Demand annotation: a local slot's value on the regions' ports.
//!
//! Construction ends with γ and θ nodes carrying *value* ports, computed from
//! what each region demands (Reissmann et al. §4). What the frontend hands over
//! stops one step short: control structure is there, but every local is still a
//! stack slot and every read of it a load. This pass finishes the construction.
//!
//! A slot whose address never leaves the function and whose accesses all name it
//! whole is not memory at all. The region that reads it takes its value as an
//! argument, the region that writes it yields it, and a loop that does either
//! carries it across its iterations — the region tree is the dominance, so
//! nothing here computes one, and there is no φ to place. The accesses go with
//! the allocation.
//!
//! What stays a slot — an address that escapes, arithmetic reaching part of it,
//! accesses disagreeing on a type — keeps every access and reaches
//! [`thread_state`](super::thread_state) as the memory it is.

use crate::analysis::scopes::{exit_scope, loop_scope, nested_exit_scopes, region_exit};
use crate::analysis::slots::{SlotState, agreed_value_type, collect_slots};
use crate::analysis::{AnalysisManager, EscapeFacts};
use crate::func::FuncOp;
use crate::{
    BlockHandle, Context, MemoryRead, MemoryWrite, OpHandle, OpId, OpInstance, OperationRef, Pass,
    PassError, PassTarget, RegionId, Rewriter, TypeId, ValueId, scf,
};

#[derive(Default)]
pub struct PromotePass;

impl PromotePass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(PromotePass, "promote");

impl Pass for PromotePass {
    fn name(&self) -> &'static str {
        "promote"
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
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        let [entry] = context.get_region(body).block_ids()[..] else {
            return Ok(());
        };
        let ops = region_ops(context, body);
        let escapes = analyses.get::<EscapeFacts>(context, op.op().id);
        for (slot, state) in collect_slots(context, &escapes, &ops) {
            let Some(ty) = promotable(context, slot, &state, entry) else {
                continue;
            };
            let mut promoter = Promoter {
                context,
                slot,
                ty,
                template: state.loads.first().copied(),
                scopes: Vec::new(),
                stand_in: None,
                dead: Vec::new(),
            };
            promoter.walk(&context.get_block(entry), None);
            promoter.finish(state.alloca, rewriter)?;
        }
        Ok(())
    }
}

/// The type a slot's value takes once promoted, or `None` where it stays memory.
///
/// The slot has to be one the walk can carry: allocated once, where the walk
/// starts, so no iteration opens a second one; named by every access whole, so
/// the value stands for the whole slot; agreed on one type, so it has a
/// spelling; and reached only through regions a port crosses.
fn promotable(
    context: &Context,
    slot: ValueId,
    state: &SlotState,
    entry: crate::BlockId,
) -> Option<TypeId> {
    let alloca = state.alloca?;
    if state.escapes || context.parent_block(alloca) != Some(entry) {
        return None;
    }
    let accesses = state.loads.iter().chain(&state.stores);
    if accesses
        .clone()
        .any(|&op| !names_whole_slot(context, op, slot))
    {
        return None;
    }
    let ty = agreed_value_type(context, state)?;
    carries_ports(context, &context.get_block(entry), slot).then_some(ty)
}

/// Whether `op` reads or writes `slot` at the slot's own address, rather than at
/// somewhere inside it that pointer arithmetic reached — and does so with no
/// chain threaded through it. Construction runs before
/// [`thread_state`](super::thread_state): an access already on a chain has
/// readers of the state it publishes, which erasing it would leave holding a
/// definition that is gone.
fn names_whole_slot(context: &Context, op: OpId, slot: ValueId) -> bool {
    let instance = context.get_op(op);
    if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() {
        return read.read_location() == slot && read.state_operand().is_none();
    }
    match instance.clone().as_interface::<dyn MemoryWrite>() {
        Some(write) => write.write_location() == slot && write.state_operand().is_none(),
        None => false,
    }
}

/// Whether every operation whose regions hold an access of `slot` is one a value
/// port crosses: a gate or a loop, each of whose regions is a single block.
fn carries_ports(context: &Context, block: &BlockHandle, slot: ValueId) -> bool {
    block.op_ids().iter().all(|&op_id| {
        let op = context.get_op(op_id);
        if op.regions().is_empty() || !accesses_under(context, &op, slot) {
            return true;
        }
        if !(op.is::<scf::ForOp>()
            || op.is::<scf::WhileOp>()
            || op.is::<scf::IfOp>()
            || op.is::<scf::SwitchOp>())
        {
            return false;
        }
        op.regions().iter().all(|&region| {
            let [entry] = context.get_region(region).block_ids()[..] else {
                return false;
            };
            carries_ports(context, &context.get_block(entry), slot)
        })
    })
}

struct Promoter<'a> {
    context: &'a Context,
    slot: ValueId,
    ty: TypeId,
    /// A load of the slot to copy for a read of what nothing wrote.
    template: Option<OpId>,
    /// The token scopes of the loops whose port is being grown, innermost last.
    scopes: Vec<ValueId>,
    /// The value standing for the slot's contents where nothing has written it.
    stand_in: Option<ValueId>,
    /// The accesses the walk answered, which the value it carries replaces.
    dead: Vec<OpId>,
}

impl Promoter<'_> {
    /// Walk `block` holding `current` — the slot's value where the walk stands,
    /// `None` before anything wrote it — and answer what it holds at the end.
    fn walk(&mut self, block: &BlockHandle, mut current: Option<ValueId>) -> Option<ValueId> {
        for op_id in block.op_ids() {
            let op = self.context.get_op(op_id);
            if let Some(written) = self.writes(&op) {
                self.dead.push(op_id);
                current = Some(written);
            } else if let Some(read) = self.reads(&op) {
                let value = self.held(current);
                self.context.replace_value_uses(read, value);
                self.dead.push(op_id);
                current = Some(value);
            } else if exit_scope(&op).is_some_and(|scope| self.scopes.contains(&scope)) {
                let value = self.held(current);
                self.context.append_port_operand(op_id, value);
                current = Some(value);
            } else if !op.regions().is_empty()
                && (accesses_under(self.context, &op, self.slot) || self.exits_under(&op))
            {
                current = self.regions(&op, current);
            }
        }
        current
    }

    /// Carry the slot's value across `op`'s regions.
    ///
    /// A region that only reads the slot reads what is in scope where the op
    /// sits, so nothing is carried: an invariant value is already spelled where
    /// the body can see it. A region that writes needs a port — the arms of a
    /// gate answer with what each yields, and a loop is entered on the value
    /// before it and left with the one its last iteration latched.
    fn regions(&mut self, op: &OpHandle, current: Option<ValueId>) -> Option<ValueId> {
        if !writes_under(self.context, op, self.slot) || !self.demanded(op) {
            return self.walk_inside(op, current);
        }
        match op.clone().as_interface::<dyn crate::LoopLike>() {
            Some(_) => Some(self.grow_theta(op, current)),
            None => self.grow_gamma(op, current),
        }
    }

    /// A gate's port: every arm that falls out of the gate yields the value the
    /// slot holds where it ends, an arm that leaves the enclosing loop yields
    /// nothing, and the gate's result is the value after it. A gate no arm falls
    /// out of publishes nothing anything after it could read.
    fn grow_gamma(&mut self, op: &OpHandle, current: Option<ValueId>) -> Option<ValueId> {
        let context = self.context;
        let (ty, op_id) = (self.ty, op.id);
        if op
            .regions()
            .iter()
            .all(|&region| region_exit(context, region).is_some())
        {
            return self.walk_inside(op, current);
        }
        Some(context.grow_port(op_id, ty, None, |region, _| {
            let entry = self.entry(region);
            let leaving = self.walk(&entry, current);
            if region_exit(context, region).is_some() {
                return None;
            }
            Some(match leaving {
                Some(value) => value,
                None => self.held(None),
            })
        }))
    }

    /// A loop's port: it is entered on the value before the loop, every region
    /// reads it as an argument, and every edge back into it — the body's latch,
    /// and each `break`/`continue` leaving the loop's scope — carries what the
    /// slot holds there. The loop's result is what it was left with.
    fn grow_theta(&mut self, op: &OpHandle, current: Option<ValueId>) -> ValueId {
        let context = self.context;
        let (ty, op_id) = (self.ty, op.id);
        let init = self.held(current);
        let scope = op
            .regions()
            .last()
            .and_then(|&body| loop_scope(context, body));
        self.scopes.extend(scope);
        let result = context.grow_port(op_id, ty, Some(init), |region, incoming| {
            let incoming = incoming.expect("a loop port is entered on a value");
            let entry = self.entry(region);
            let leaving = self.walk(&entry, Some(incoming));
            // A region left through an exit fed the port along that edge.
            region_exit(context, region)
                .is_none()
                .then(|| leaving.unwrap_or(incoming))
        });
        if scope.is_some() {
            self.scopes.pop();
        }
        result
    }

    /// Walk `op`'s regions on the value the slot holds where `op` sits, carrying
    /// nothing across it: what a region reads is in scope there already, and what
    /// one writes reaches nothing outside it.
    fn walk_inside(&mut self, op: &OpHandle, current: Option<ValueId>) -> Option<ValueId> {
        for region in op.regions().to_vec() {
            let entry = self.entry(region);
            self.walk(&entry, current);
        }
        current
    }

    /// The value the slot holds, standing in for what nothing wrote with a read
    /// of the untouched allocation. Nothing names an indeterminate value, and the
    /// allocation is exactly the memory the reader would have read.
    fn held(&mut self, current: Option<ValueId>) -> ValueId {
        if let Some(value) = current.or(self.stand_in) {
            return value;
        }
        let template = self
            .context
            .get_op(self.template.expect("a read to stand in for"));
        let results: Vec<ValueId> = template
            .results()
            .iter()
            .map(|&result| {
                self.context
                    .create_value(self.context.get_value(result).ty(), None)
                    .id()
            })
            .collect();
        let copy = self.context.add_operation(OpInstance::new_dynamic(
            (template.dialect().as_str(), template.name().as_str()),
            self.context.as_context_ref(),
            template.operands().to_vec(),
            results.clone(),
            vec![],
            template.attributes().to_vec(),
        ));
        let allocation = self
            .context
            .get_value(self.slot)
            .defining_op()
            .expect("a slot is allocated");
        let block = self
            .context
            .get_block(self.context.parent_block(allocation).expect("in a block"));
        let index = block
            .op_ids()
            .iter()
            .position(|&op| op == allocation)
            .expect("the allocation is in its own block");
        block.insert(index + 1, copy.id);
        self.stand_in = Some(results[0]);
        results[0]
    }

    /// Erase what the walk answered: the accesses the carried value replaced, and
    /// the allocation, unless a read of nothing stands on it.
    fn finish(&mut self, alloca: Option<OpId>, rewriter: &mut Rewriter) -> Result<(), PassError> {
        let dead = std::mem::take(&mut self.dead);
        // A read of what nothing wrote stands on the allocation, so that one stays.
        let swept = alloca.filter(|_| self.stand_in.is_none());
        for op_id in dead.into_iter().chain(swept) {
            let block = self
                .context
                .parent_block(op_id)
                .map(|id| self.context.get_block(id));
            rewriter.erase_op(&OperationRef::new(self.context.get_op(op_id), block, None))?;
        }
        Ok(())
    }

    /// The value a write to the slot leaves it holding.
    fn writes(&self, op: &OpHandle) -> Option<ValueId> {
        let write = op.clone().as_interface::<dyn MemoryWrite>()?;
        (write.write_location() == self.slot).then(|| write.written_value())
    }

    /// The value a read of the slot yields, which the carried one replaces.
    fn reads(&self, op: &OpHandle) -> Option<ValueId> {
        let read = op.clone().as_interface::<dyn MemoryRead>()?;
        (read.read_location() == self.slot).then(|| read.read_value())
    }

    /// Whether a port on `op` carries the value anywhere: a region of it reads
    /// the slot before writing it, so what the op is entered with is demanded
    /// inside, or something after the op reads what it left. A write nobody can
    /// observe needs no port, however deeply it is nested.
    fn demanded(&self, op: &OpHandle) -> bool {
        demands_regions(self.context, op, self.slot) || live_after(self.context, op.id, self.slot)
    }

    /// Whether an exit leaving a loop whose port is being grown sits inside `op`.
    fn exits_under(&self, op: &OpHandle) -> bool {
        nested_exit_scopes(self.context, op)
            .iter()
            .any(|scope| self.scopes.contains(scope))
    }

    fn entry(&self, region: RegionId) -> BlockHandle {
        self.context
            .get_block(self.context.get_region(region).block_ids()[0])
    }
}

/// Whether any region of `op` demands the value it is entered with.
fn demands_regions(context: &Context, op: &OpHandle, slot: ValueId) -> bool {
    op.regions().iter().any(|&region| {
        let [entry] = context.get_region(region).block_ids()[..] else {
            return true;
        };
        demands(context, &context.get_block(entry), slot)
    })
}

/// Whether some path through `block` reads `slot` before writing it, so the
/// value the block is entered with is demanded inside it. A region-holding op
/// kills nothing: whether every path through it writes is not a question the
/// walk asks, and carrying a value nobody reads costs a port, not a fact.
fn demands(context: &Context, block: &BlockHandle, slot: ValueId) -> bool {
    for op_id in block.op_ids() {
        let op = context.get_op(op_id);
        if writes_to(&op, slot) {
            return false;
        }
        if reads_from(&op, slot) {
            return true;
        }
        if demands_regions(context, &op, slot) {
            return true;
        }
    }
    false
}

/// Whether anything reads what `op` leaves in `slot`: later in its own block
/// before a write puts something else there, anywhere in a loop it sits inside —
/// the back edge reaches the reads before it too — or after whichever operation
/// holds that loop.
fn live_after(context: &Context, op: OpId, slot: ValueId) -> bool {
    let Some(block) = context.parent_block(op) else {
        return false;
    };
    let handle = context.get_block(block);
    let ops = handle.op_ids();
    let index = ops.iter().position(|&id| id == op).unwrap_or(0);
    for &later in &ops[index + 1..] {
        if reads_at_or_under(context, later, slot) {
            return true;
        }
        // A write here runs whatever happened before it, so nothing after it can
        // read what `op` left.
        if writes_to(&context.get_op(later), slot) {
            return false;
        }
    }
    let Some(holder) = context
        .parent_region(block)
        .and_then(|region| context.get_region(region).parent_op())
    else {
        return false;
    };
    let instance = context.get_op(holder);
    if instance.has_interface::<dyn crate::LoopLike>() && demands_regions(context, &instance, slot)
    {
        return true;
    }
    live_after(context, holder, slot)
}

fn reads_at_or_under(context: &Context, op: OpId, slot: ValueId) -> bool {
    let instance = context.get_op(op);
    reads_from(&instance, slot)
        || subtree_ops(context, &instance).any(|inner| reads_from(&context.get_op(inner), slot))
}

fn reads_from(op: &OpHandle, slot: ValueId) -> bool {
    op.clone()
        .as_interface::<dyn MemoryRead>()
        .is_some_and(|read| read.read_location() == slot)
}

fn writes_to(op: &OpHandle, slot: ValueId) -> bool {
    op.clone()
        .as_interface::<dyn MemoryWrite>()
        .is_some_and(|write| write.write_location() == slot)
}

/// Whether anything in `op`'s region tree accesses `slot`.
fn accesses_under(context: &Context, op: &OpHandle, slot: ValueId) -> bool {
    subtree_ops(context, op).any(|op_id| {
        let instance = context.get_op(op_id);
        reads_from(&instance, slot) || writes_to(&instance, slot)
    })
}

/// Whether anything in `op`'s region tree writes `slot`.
fn writes_under(context: &Context, op: &OpHandle, slot: ValueId) -> bool {
    subtree_ops(context, op).any(|op_id| writes_to(&context.get_op(op_id), slot))
}

fn subtree_ops(context: &Context, op: &OpHandle) -> impl Iterator<Item = OpId> {
    op.regions()
        .to_vec()
        .into_iter()
        .flat_map(|region| region_ops(context, region))
}

fn region_ops(context: &Context, region: RegionId) -> Vec<OpId> {
    let mut ops = Vec::new();
    for block in context.get_region(region).iter(context.clone()) {
        for op_id in block.op_ids() {
            ops.push(op_id);
            for nested in context.get_op(op_id).regions() {
                ops.extend(region_ops(context, nested));
            }
        }
    }
    ops
}
