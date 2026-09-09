//! The simplifier over unordered regions: the same e-graph, seeded with the
//! declared loops and gates, committed without an order to place into.
//!
//! A rewrite hands a value's readers the cheapest spelling of its class. What
//! that spelling is in scope of is the region tree alone: a value defined in
//! the reader's region or one enclosing it is read as it is; one defined in an
//! arm the reader cannot see is rebuilt where the reader sits, if the operation
//! computing it cannot trap, and left where it is otherwise. A rule-introduced
//! op or a literal is built and placed where its operands say. Nothing here
//! asks where an operation sits in its region or which came first.
//!
//! The commit erases nothing on the way. The sweep after it erases every
//! operation the region's results do not demand. Demand runs through
//! dependencies as through values, so a loop whose body changes memory is
//! demanded by the state it leaves however unread its values are; a loop in
//! no cone at all never ran under the demand semantics, and goes.

use std::collections::{HashMap, HashSet};

/// The spelling a class already has, so a form rebuilt for one reader answers
/// every reader that can see it. `add_auto` places a rebuilt form where its
/// operands allow, which may be well outside the reader's own region.
type Memo = HashMap<Id, ValueId>;

use tir_relational::{ClassId as Id, Extraction};

use super::{Driver, Node, Prov, SymKind, cost, state};
use crate::analysis::AnalysisManager;
use crate::analysis::effects::{observed_state, produced_state};
use crate::binding::{StateChain, forwards_state, state_chains};
use crate::func::FuncOp;
use crate::sem::egraph::type_width;
use crate::{
    ConstantLike, Context, Gamma, MemoryRead, MemoryWrite, NewOp, OpHandle, OpId, OperationRef,
    Pass, PassError, PassTarget, PromotableAllocation, RegionId, RegionKind, Rewriter,
    Speculatable, Theta, TypeId, ValueId,
};

#[derive(Default)]
pub struct InstCombineNodesPass;

impl InstCombineNodesPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(InstCombineNodesPass, "instcombine-nodes");

impl Pass for InstCombineNodesPass {
    fn name(&self) -> &'static str {
        "instcombine-nodes"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Nodes)
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let root = op.op().id;
        let mut seeded = super::seed::seed(context, root);
        let loop_ports = std::mem::take(&mut seeded.loop_ports);
        let ruleset = super::builtin_ruleset(context, &seeded);
        let mut driver = Driver {
            context,
            eg: seeded.eg,
            value_class: seeded.value_class,
            ruleset,
            replacing: std::cell::Cell::new(None),
        };
        driver.saturate();
        driver.hypothesize(loop_ports);
        let extraction = driver.eg.extract_best(|_, node| cost(node));
        let body = context.get_op(root).regions()[0];
        driver.commit_nodes(body, &extraction, &mut HashMap::new())?;
        forget_write_only_slots(context, body);
        drop_untouched_chains(context, body);
        let result = sweep(context, body, rewriter);
        tir_relational::report_saturation("instcombine-nodes");
        result
    }
}

impl Driver<'_> {
    /// Rewire every port and every value result defined in `region`, then the
    /// regions nested in it, each arm of a boolean gate under the fact its
    /// predicate states there.
    fn commit_nodes(
        &mut self,
        region: RegionId,
        extraction: &Extraction<'_, Node>,
        memo: &mut Memo,
    ) -> Result<(), PassError> {
        let handle = self.context.get_region(region);
        for port in handle.value_arguments() {
            self.rewire_nodes(port.id(), region, extraction, memo)?;
        }
        let ops = handle.op_ids();
        for &op in &ops {
            let instance = self.context.get_op(op);
            if instance.has_interface::<dyn ConstantLike>() {
                continue;
            }
            if instance.is::<crate::state::JoinOp>()
                && let Some(merged) = self.merged_state(&instance)
            {
                for result in instance.state_results() {
                    self.context.replace_value_uses(result, merged);
                    self.context
                        .rename_region_results(region, result, merged, &[]);
                }
                continue;
            }
            for result in instance.value_results() {
                let replaced = self.rewire_nodes(result, region, extraction, memo)?;
                if replaced {
                    self.forward_read_state(&instance, region);
                }
            }
        }
        // Once every read the rewrites answered has let go of its state, a
        // write nothing else observes is unread. Finding the regions a result
        // list can sit in walks every operation under `region`, and nothing in
        // this loop adds or removes one, so it is found once.
        let scope = self.context.nested_regions(region);
        for &op in &ops {
            self.forward_dead_write(op, &scope);
        }
        for &op in &ops {
            let instance = self.context.get_op(op);
            let arms = instance
                .clone()
                .as_interface::<dyn Gamma>()
                .filter(|gamma| {
                    gamma.arms().len() == 2
                        && type_width(self.context, self.context.get_value(gamma.predicate()).ty())
                            == Some(1)
                })
                .map(|gamma| (gamma.predicate(), gamma.arms()));
            match arms {
                Some((predicate, arms)) => {
                    for (index, arm) in arms.into_iter().enumerate() {
                        self.eg.push_context();
                        self.inject(predicate, index == 1);
                        self.saturate();
                        let dirty = self.eg.innermost_dirty();
                        let scoped = extraction.refresh(&self.eg, &dirty, |_, node| cost(node));
                        // A spelling built under this arm's fact answers only here.
                        let mut scoped_memo = memo.clone();
                        self.commit_nodes(arm, &scoped, &mut scoped_memo)?;
                        self.eg.pop_context();
                    }
                }
                None => {
                    for sub in instance.regions() {
                        self.commit_nodes(sub, extraction, memo)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The one state a join names, where it names one: the state its inputs all
    /// are, or the one chain among them something changed.
    ///
    /// Dropping an input is only the join's to do where the join is that
    /// input's one reader: another reader would be left sharing the state with
    /// whatever reads the join, which is a fork the discipline may forbid.
    fn merged_state(&self, join: &OpHandle) -> Option<ValueId> {
        let operands = join.state_operands();
        let (first, rest) = operands.split_first()?;
        if rest.iter().all(|other| other == first) {
            return Some(*first);
        }
        if !operands
            .iter()
            .all(|&state| self.context.users_of(state).as_slice() == [join.id])
        {
            return None;
        }
        let mut changed = operands
            .iter()
            .copied()
            .filter(|&state| changed_chain(self.context, state));
        let kept = changed.next();
        changed.next().is_none().then(|| kept.unwrap_or(*first))
    }

    /// A write nothing observes before the next write of its own extent is
    /// overwritten unread: its readers take the state it was handed, and the
    /// sweep takes it. The walk follows that write's chain alone, since the
    /// names a split and a join give it on the way are the same memory.
    fn forward_dead_write(&self, op: OpId, scope: &[RegionId]) {
        let instance = self.context.get_op(op);
        if !instance.has_interface::<dyn MemoryWrite>() {
            return;
        }
        let (Some(taken), Some(leaves)) = (observed_state(&instance), produced_state(&instance))
        else {
            return;
        };
        let Some(extent) = self.extent(leaves) else {
            return;
        };
        let mut state = leaves;
        loop {
            if published(self.context, scope, state) {
                return;
            }
            let readers: Vec<OpId> = self
                .context
                .users_of(state)
                .into_iter()
                .filter(|&reader| !is_dead_read(self.context, scope, reader))
                .collect();
            let [reader] = readers[..] else {
                return;
            };
            let handle = self.context.get_op(reader);
            // The write's own chain is the one it named first, so it is the
            // first the split hands back; a merge names the memory the change
            // taking it observes.
            if handle.is::<crate::state::SplitOp>() && state == handle.state_operands()[0]
                || handle.is::<crate::state::JoinOp>()
            {
                state = handle.state_results()[0];
                continue;
            }
            if !handle.has_interface::<dyn MemoryWrite>() {
                return;
            }
            let (Some(observed), Some(left)) = (observed_state(&handle), produced_state(&handle))
            else {
                return;
            };
            if observed != state {
                return;
            }
            if self.extent(left) != Some(extent) {
                return;
            }
            break;
        }
        // The loop's first guard found no region result naming the state the
        // write leaves, so no result list here names it either.
        debug_assert!(!published(self.context, scope, leaves));
        self.context.replace_value_uses(leaves, taken);
    }

    /// The extent the write publishing `state` covers: the object its address
    /// derives from, the offset into it and the byte count.
    fn extent(&self, state: ValueId) -> Option<(Id, i64, u64)> {
        let class = self.eg.find(*self.value_class.get(&state)?);
        let node = self
            .eg
            .nodes(class)
            .find(|node| node.prov == Prov::Value(state))?;
        if node.sym() != Some(SymKind::StoreMemory) || node.children.len() != state::STORE_ARITY {
            return None;
        }
        let (object, offset) = self.eg.object_of(node.children[state::ADDRESS])?;
        let bytes = self
            .eg
            .nodes(self.eg.find(node.children[state::BYTES]))
            .find_map(|node| node.int())?;
        Some((self.eg.find(object), offset, bytes.to_u64()))
    }

    /// A read whose value was rewritten leaves memory as it found it: the state
    /// it published is the state it observed, so its readers take that, and the
    /// read is demanded by nothing.
    fn forward_read_state(&self, instance: &OpHandle, region: RegionId) {
        if !instance.has_interface::<dyn MemoryRead>() {
            return;
        }
        if let (Some(observed), Some(published)) =
            (observed_state(instance), produced_state(instance))
        {
            self.context.replace_value_uses(published, observed);
            self.context
                .rename_region_results(region, published, observed, &[]);
        }
    }

    /// Hand the readers of `value`, defined in `region`, the cheapest spelling
    /// of its class where that spelling is one the region can see. Answers
    /// whether anything changed.
    fn rewire_nodes(
        &mut self,
        value: ValueId,
        region: RegionId,
        extraction: &Extraction<'_, Node>,
        memo: &mut Memo,
    ) -> Result<bool, PassError> {
        let Some(&class) = self.value_class.get(&value) else {
            return Ok(false);
        };
        if !self.context.is_used(value)
            && !published(self.context, &self.context.nested_regions(region), value)
        {
            return Ok(false);
        }
        let ty = self.context.get_value(value).ty();
        self.replacing.set(Some(value));
        let spelled = self.materialize_nodes(extraction, class, ty, region, memo);
        self.replacing.set(None);
        let Some(new_value) = spelled else {
            return Ok(false);
        };
        if new_value == value {
            return Ok(false);
        }
        self.context.replace_value_uses(value, new_value);
        // A port's own region may pin a result entry to the port — a counted
        // loop's exit is its port — so the entries a port's region names stay;
        // the regions nested in it read the port like any other value.
        if self.context.region_of_port(value) == Some(region) {
            for op in self.context.get_region(region).op_ids() {
                for sub in self.context.get_op(op).regions() {
                    self.context
                        .rename_region_results(sub, value, new_value, &[]);
                }
            }
        } else {
            self.context
                .rename_region_results(region, value, new_value, &[]);
        }
        Ok(true)
    }

    /// The value of `class`'s cheapest node where `region` can read it, or
    /// `None` where that node has no spelling there: the value being rewritten
    /// itself, or an op that may trap sitting in a region the reader cannot see.
    fn materialize_nodes(
        &self,
        extraction: &Extraction<'_, Node>,
        class: Id,
        expected_ty: TypeId,
        region: RegionId,
        memo: &mut Memo,
    ) -> Option<ValueId> {
        let class = self.eg.find(class);
        if let Some(&value) = memo.get(&class)
            && visible(self.context, value, region)
        {
            return Some(value);
        }
        let node = extraction.node(class)?;
        let value = match node.prov {
            Prov::Value(_) | Prov::Op(_) => {
                let named = self.named_value(node)?;
                if Some(named) == self.replacing.get() {
                    return None;
                }
                if visible(self.context, named, region) {
                    named
                } else {
                    self.rebuild(extraction, node, region, memo)?
                }
            }
            Prov::Introduced(idx) => {
                let ty = node.ty.expect("an op node carries its result type");
                let types = vec![ty; node.children.len()];
                let operands = self.materialize_children(extraction, node, &types, region, memo)?;
                let emit = self.ruleset.emits[idx]
                    .as_ref()
                    .expect("an introduced op supplies an emit");
                let (op, value) = emit(self.context, &operands, ty);
                self.context.add_auto(op.id());
                value
            }
            Prov::None => {
                let literal = node.int()?;
                let op =
                    crate::builtin::ops::constant(self.context, super::spell(literal), expected_ty)
                        .build();
                self.context.add(region, crate::Operation::id(&op));
                op.result()
            }
        };
        memo.insert(class, value);
        Some(value)
    }

    /// Spell a seeded op's value where `region` can read it: a copy of the op
    /// over its operands' spellings, placed where they say. Only an operation
    /// that cannot trap may run where the original would not have, and one
    /// without operands names no region to join.
    fn rebuild(
        &self,
        extraction: &Extraction<'_, Node>,
        node: &Node,
        region: RegionId,
        memo: &mut Memo,
    ) -> Option<ValueId> {
        let Prov::Op(op) = node.prov else {
            return None;
        };
        let source = self.context.get_op(op);
        if !source.has_interface::<dyn Speculatable>()
            || !source.regions().is_empty()
            || !source.state_operands().is_empty()
            || source.operands().is_empty()
            || source.results().len() != 1
        {
            return None;
        }
        let ty = node.ty?;
        // Each operand is spelled at the type the original read it at: a
        // comparison's operands are not its boolean.
        let types: Vec<TypeId> = source
            .operands()
            .iter()
            .map(|&operand| self.context.get_value(operand).ty())
            .collect();
        if types.len() != node.children.len() {
            return None;
        }
        let operands = self.materialize_children(extraction, node, &types, region, memo)?;
        let result = self.context.create_value(ty, None).id();
        let copy = self.context.add_operation(NewOp::new_dynamic(
            (source.dialect().as_str(), source.name().as_str()),
            self.context.as_context_ref(),
            operands,
            vec![result],
            vec![],
            source.attributes().to_vec(),
        ));
        self.context.add_auto(copy.id);
        Some(result)
    }

    fn materialize_children(
        &self,
        extraction: &Extraction<'_, Node>,
        node: &Node,
        types: &[TypeId],
        region: RegionId,
        memo: &mut Memo,
    ) -> Option<Vec<ValueId>> {
        node.children
            .iter()
            .zip(types)
            .map(|(&arg, &ty)| self.materialize_nodes(extraction, arg, ty, region, memo))
            .collect()
    }
}

/// Whether anything changed the memory `state` names since its chain opened.
/// An entry state names the memory the region was entered with; a loop or a
/// gate hands back what it was entered on wherever its regions leave that
/// chain's port alone.
fn changed_chain(context: &Context, state: ValueId) -> bool {
    let Some(def) = context.get_value(state).defining_op() else {
        return true;
    };
    let instance = context.get_op(def);
    if instance.is::<crate::state::EntryStateOp>() {
        return false;
    }
    // The chains a function opens are one entry state split, so a split of a
    // memory nothing changed names one nothing changed either.
    if instance.is::<crate::state::SplitOp>() {
        return changed_chain(context, instance.state_operands()[0]);
    }
    let Some(chain) = state_chains(context, &instance)
        .into_iter()
        .find(|chain| chain.left == state)
    else {
        return true;
    };
    if !forwards_state(&chain) {
        return true;
    }
    changed_chain(context, chain.entered)
}

/// Whether `region` can read `value`: it is defined in `region` or in one
/// enclosing it, by an operation other than the one carrying `region`, whose
/// results are what its regions produce.
fn visible(context: &Context, value: ValueId, region: RegionId) -> bool {
    let Some(defined) = crate::region::defining_region(context, value) else {
        return true;
    };
    let defining_op = context.get_value(value).defining_op();
    let mut current = Some(region);
    while let Some(here) = current {
        if here == defined {
            return true;
        }
        let carrier = context.get_region(here).parent_op();
        if carrier.is_some() && carrier == defining_op {
            return false;
        }
        current = carrier.and_then(|op| context.region_of_op(op));
    }
    false
}

/// A loop or a gate carrying a chain its body never names carries nothing: the
/// port hands back the memory the operation was entered on, so it goes with the
/// chain. What leaves such a port behind is promotion, whose slot values move
/// onto the value ports and leave the chain empty.
fn drop_untouched_chains(context: &Context, region: RegionId) {
    for op in context.get_region(region).op_ids() {
        for sub in context.get_op(op).regions() {
            drop_untouched_chains(context, sub);
        }
        let instance = context.get_op(op);
        // A split names one memory per chain crossing it, so where one chain is
        // left it names that memory and nothing else: its reader takes the
        // state the split was handed.
        if instance.is::<crate::state::SplitOp>() {
            let read: Vec<ValueId> = instance
                .state_results()
                .into_iter()
                .filter(|&state| {
                    !context.users_of(state).is_empty()
                        || published(context, &context.nested_regions(region), state)
                })
                .collect();
            if let [kept] = read[..] {
                let taken = instance.state_operands()[0];
                context.replace_value_uses(kept, taken);
                context.rename_region_results(region, kept, taken, &[]);
            }
            continue;
        }
        // Highest chain first, so the ports that stay keep their positions.
        for ordinal in (0..state_chains(context, &instance).len()).rev() {
            let Some(chain) = carries_nothing(context, op, ordinal) else {
                continue;
            };
            context.replace_value_uses(chain.left, chain.entered);
            context.rename_region_results(region, chain.left, chain.entered, &[]);
            context.drop_state(op, ordinal);
        }
    }
}

/// The `ordinal`-th chain `op` carries, if nothing under the op names it:
/// every region hands the port straight back, and nothing else reads it.
fn carries_nothing(context: &Context, op: OpId, ordinal: usize) -> Option<StateChain> {
    let handle = context.get_op(op);
    let chain = state_chains(context, &handle).swap_remove(ordinal);
    if !forwards_state(&chain) {
        return None;
    }
    let groups = if handle.has_interface::<dyn Theta>() {
        2
    } else {
        1
    };
    let unnamed = handle
        .regions()
        .iter()
        .zip(&chain.ports)
        .all(|(&region, &port)| {
            let named = context
                .nested_regions(region)
                .iter()
                .flat_map(|&nested| context.get_region(nested).results())
                .filter(|&named| named == port)
                .count();
            named == groups && context.users_of(port).is_empty()
        });
    unnamed.then_some(chain)
}

/// A slot whose address reaches only writes is a memory nothing observes: each
/// write leaves the state it was handed, and the sweep takes the writes and
/// the allocation with nothing left demanding them.
fn forget_write_only_slots(context: &Context, body: RegionId) {
    let scope = context.nested_regions(body);
    let mut renames: HashMap<ValueId, ValueId> = HashMap::new();
    for op in context.get_region(body).op_ids() {
        let instance = context.get_op(op);
        let Some(allocation) = instance.clone().as_interface::<dyn PromotableAllocation>() else {
            continue;
        };
        let mut writes = Vec::new();
        if !only_written(context, &scope, allocation.promoted_location(), &mut writes) {
            continue;
        }
        for write in writes {
            let instance = context.get_op(write);
            if !instance.has_interface::<dyn MemoryWrite>() {
                continue;
            }
            let (Some(taken), Some(published)) =
                (observed_state(&instance), produced_state(&instance))
            else {
                continue;
            };
            context.replace_value_uses(published, taken);
            renames.insert(published, taken);
        }
    }
    context.rename_region_results_batch(body, &renames);
}

/// A read nothing demands: its value and state are read by nothing, so the
/// sweep takes it and the state it observed has one reader fewer.
fn is_dead_read(context: &Context, scope: &[RegionId], op: OpId) -> bool {
    let instance = context.get_op(op);
    instance.has_interface::<dyn MemoryRead>()
        && !instance.has_interface::<dyn MemoryWrite>()
        && instance.results().iter().all(|&result| {
            context.users_of(result).is_empty() && !published(context, scope, result)
        })
}

/// Whether any region in `scope` publishes `value` as a result, which no use
/// list says. Only a region that can see `value` names it, so a `scope` wider
/// than the one it is defined in answers the same.
fn published(context: &Context, scope: &[RegionId], value: ValueId) -> bool {
    scope
        .iter()
        .any(|&region| context.get_region(region).results().contains(&value))
}

/// Whether every use of `address`, through pointer arithmetic, is as the
/// location of a write or of a read nothing demands; the writes are collected.
fn only_written(
    context: &Context,
    scope: &[RegionId],
    address: ValueId,
    writes: &mut Vec<OpId>,
) -> bool {
    context.users_of(address).into_iter().all(|user| {
        if is_dead_read(context, scope, user) {
            return true;
        }
        let instance = context.get_op(user);
        if instance.is::<crate::ptr::PtrAddOp>() {
            return instance
                .results()
                .iter()
                .all(|&derived| only_written(context, scope, derived, writes));
        }
        match instance.clone().as_interface::<dyn MemoryWrite>() {
            // The address is the write's location and nothing else: a write
            // storing the address itself lets it out.
            Some(write)
                if write.write_location() == address
                    && !instance.has_interface::<dyn MemoryRead>()
                    && instance
                        .operands()
                        .iter()
                        .filter(|&&v| v == address)
                        .count()
                        == 1 =>
            {
                writes.push(user);
                true
            }
            _ => false,
        }
    }) && !published(context, scope, address)
}

/// Erase every operation under `body` its region's results do not demand.
/// Demand runs from the callable's results through operands and nested
/// regions' results, dependencies included: what an effect leaves behind is
/// what demands it.
fn sweep(context: &Context, body: RegionId, rewriter: &mut Rewriter) -> Result<(), PassError> {
    let demanded = crate::passes::demanded_ops(context, &[body]);
    sweep_region(context, body, &demanded, rewriter)
}

fn sweep_region(
    context: &Context,
    region: RegionId,
    demanded: &HashSet<OpId>,
    rewriter: &mut Rewriter,
) -> Result<(), PassError> {
    for op in context.get_region(region).op_ids() {
        if !demanded.contains(&op) {
            rewriter.erase_op(&OperationRef::new(context.get_op(op)))?;
            continue;
        }
        for sub in context.get_op(op).regions() {
            sweep_region(context, sub, demanded, rewriter)?;
        }
    }
    Ok(())
}
