//! Seeds the e-graph by reading a function's regions. Gates come off the ops'
//! own interfaces, never from control-flow analysis: a [`Conditional`] result is
//! the γ over the values its arms yield, and an arm's entry arguments are the
//! inputs forwarded into it. A memory access is the `LoadMemory`/`StoreMemory`
//! term over the state it reads, so the chain is an ordinary edge. Everything
//! else the vocabulary cannot spell — a loop's carried port and results, a
//! multi-result or effectful op — anchors as an input leaf.

use std::collections::{HashMap, HashSet};

use tir_symbolic::egraph::{EGraph, Id};

use crate::analysis::scopes::{carried_operands, port_edges, region_exit, tested_ports};
use crate::analysis::slots::{collect_slots, values_agree_on_type};
use crate::builtin::StateType;
use crate::sem::egraph::{minimal_unsigned_apint, type_width};
use crate::sem::{Prov, SemNode as Node, SymKind};
use crate::{
    BlockId, Commutative, Conditional, ConstantLike, Context, LoopLike, MemoryRead, MemoryWrite,
    OpHandle, OpId, RegionId, TokenScope, TypeId, ValueId,
};

/// The operands a store term names: the location, the value it writes, and the
/// state it observes.
const STORE_OPERANDS: usize = 3;

/// The seeded e-graph plus the driver's maps: each value's class, each block
/// argument's block, the state classes something outside the term graph
/// observes, and the addresses a law may reason about as a value.
pub struct Seeded {
    pub eg: EGraph<Node>,
    pub value_class: HashMap<ValueId, Id>,
    pub arg_block: HashMap<ValueId, BlockId>,
    pub exported_states: Vec<Id>,
    pub promotable_addresses: Vec<Id>,
}

/// Build the e-graph for the regions of `root`.
pub fn seed(context: &Context, root: OpId) -> Seeded {
    let mut seeder = Seeder {
        context,
        eg: EGraph::new(),
        value_class: HashMap::new(),
        arg_block: HashMap::new(),
        seeded: HashSet::new(),
        state_ty: StateType::new(context),
        pointer_width: crate::DataLayout::for_op(context, root)
            .and_then(|layout| layout.pointer_size()),
        exported_states: Vec::new(),
        ops: Vec::new(),
    };
    for region in context.get_op(root).regions().to_vec() {
        seeder.seed_region(region);
    }
    let promotable_addresses = seeder.promotable_addresses();
    Seeded {
        eg: seeder.eg,
        value_class: seeder.value_class,
        arg_block: seeder.arg_block,
        exported_states: seeder.exported_states,
        promotable_addresses,
    }
}

struct Seeder<'a> {
    context: &'a Context,
    eg: EGraph<Node>,
    value_class: HashMap<ValueId, Id>,
    arg_block: HashMap<ValueId, BlockId>,
    seeded: HashSet<OpId>,
    state_ty: TypeId,
    pointer_width: Option<u32>,
    exported_states: Vec<Id>,
    ops: Vec<OpId>,
}

impl Seeder<'_> {
    fn seed_region(&mut self, region: RegionId) {
        let blocks: Vec<_> = self
            .context
            .get_region(region)
            .iter(self.context.clone())
            .collect();
        for block in blocks {
            for argument in block.arguments() {
                self.arg_block.insert(argument.id(), block.id());
                self.class_of(argument.id());
            }
            for op in block.op_ids() {
                self.ops.push(op);
                self.seed_op(op);
            }
        }
    }

    /// The classes of the addresses a slot-level law may treat as a value: an
    /// allocation whose pointer never leaves the load/store locations, and whose
    /// accesses agree on one type, so a value reconstructed for it has one
    /// spelling. The reading is the one every memory transform asks for.
    fn promotable_addresses(&self) -> Vec<Id> {
        collect_slots(self.context, &self.ops)
            .iter()
            .filter(|(_, slot)| {
                slot.alloca.is_some() && !slot.escapes && values_agree_on_type(self.context, slot)
            })
            .filter_map(|(address, _)| self.value_class.get(address).copied())
            .collect()
    }

    /// The class of `value`: the one already seeded for it, the one its defining
    /// op seeds, or an anchor leaf.
    fn class_of(&mut self, value: ValueId) -> Id {
        if let Some(&id) = self.value_class.get(&value) {
            return id;
        }
        if let Some(op) = self.context.get_value(value).defining_op() {
            self.seed_op(op);
        }
        self.anchor(value)
    }

    /// The leaf standing for `value`, unless something already seeded a class for it.
    fn anchor(&mut self, value: ValueId) -> Id {
        if let Some(&id) = self.value_class.get(&value) {
            return id;
        }
        let id = self.eg.add(Node::input(value));
        self.value_class.insert(value, id);
        id
    }

    /// Seed every result of `op`. Memoized: an operand whose definition the walk
    /// has not reached yet pulls it in, and the memo breaks any cycle it meets.
    fn seed_op(&mut self, op: OpId) {
        if !self.seeded.insert(op) {
            return;
        }
        let instance = self.context.get_op(op);

        if !instance.regions().is_empty() {
            self.export_states(&instance);
            if let Some(conditional) = instance.clone().as_interface::<dyn Conditional>() {
                self.seed_gamma(&instance, conditional.as_ref());
                return;
            }
            match instance.clone().as_interface::<dyn LoopLike>() {
                Some(loop_like) => self.seed_theta(&instance, loop_like.as_ref()),
                // A loop the vocabulary cannot spell carries no reasoning: its
                // ports and results anchor, but its body is read like any other
                // region.
                None => {
                    for region in instance.regions().to_vec() {
                        self.seed_region(region);
                    }
                }
            }
        } else if let (Some(constant), [result]) = (
            instance.clone().as_interface::<dyn ConstantLike>(),
            instance.results().as_slice(),
        ) {
            let ty = self.context.get_value(*result).ty();
            let id = self
                .eg
                .add(Node::constant(constant.constant_value(), Prov::Op(op)).typed(ty));
            self.value_class.insert(*result, id);
            return;
        } else if self.seed_memory(&instance) {
            return;
        } else if is_pure_value(&instance) {
            let value = instance.results()[0];
            let ty = self.context.get_value(value).ty();
            let commutative = instance.has_interface::<dyn Commutative>();
            let mut args: Vec<Id> = instance
                .operands()
                .to_vec()
                .iter()
                .map(|&operand| self.class_of(operand))
                .collect();
            if commutative {
                args.sort_by_key(|id| id.index());
            }
            let id = self.eg.add(Node::seeded(&instance, ty, commutative, args));
            self.value_class.insert(value, id);
            return;
        } else {
            self.export_states(&instance);
        }

        for result in instance.results().to_vec() {
            self.anchor(result);
        }
    }

    /// γ: each arm's entry arguments bind to the inputs forwarded into it, and
    /// result `index` is the choice between the arms' `index`-th yielded values.
    ///
    /// An arm leaving the enclosing loop never reaches what follows the gate, so
    /// what it would have yielded is never read: a gate one arm leaves through
    /// publishes what the arm that stays yields, and one every arm leaves through
    /// publishes nothing the graph can name.
    fn seed_gamma(&mut self, instance: &OpHandle, conditional: &dyn Conditional) {
        let decision = self.class_of(conditional.decision());
        for region in instance.regions().to_vec() {
            self.bind_arm_arguments(instance, region);
            self.seed_region(region);
        }
        let cases = conditional.case_values();
        let arms: Vec<RegionId> = cases
            .iter()
            .map(|&(region, _)| region)
            .filter(|&region| region_exit(self.context, region).is_none())
            .collect();
        for (index, &result) in instance.results().to_vec().iter().enumerate() {
            let yields: Option<Vec<ValueId>> = arms
                .iter()
                .map(|&region| conditional.region_yields(region).get(index).copied())
                .collect();
            match yields.as_deref() {
                Some(&[value]) => {
                    let id = self.class_of(value);
                    self.value_class.insert(result, id);
                }
                // Every arm answers, so the γ is the choice between them, one
                // child per arm in the order the cases are reported — the order
                // the commit maps back onto the regions.
                Some(values) if values.len() == cases.len() => {
                    let mut args = vec![decision];
                    for &value in values {
                        args.push(self.class_of(value));
                    }
                    let id = self.eg.add(Node::gamma(result, args));
                    self.value_class.insert(result, id);
                }
                _ => {
                    self.anchor(result);
                }
            }
        }
    }

    /// An arm's entry arguments are the inputs the gate forwards into it: the
    /// operation's trailing operands, one per argument.
    fn bind_arm_arguments(&mut self, instance: &OpHandle, region: RegionId) {
        let Some(block) = self
            .context
            .get_region(region)
            .iter(self.context.clone())
            .next()
        else {
            return;
        };
        let arguments: Vec<ValueId> = block.arguments().iter().map(|a| a.id()).collect();
        let first = instance.operands().len().saturating_sub(arguments.len());
        for (&argument, &input) in arguments.iter().zip(&instance.operands()[first..]) {
            let id = self.class_of(input);
            self.value_class.insert(argument, id);
        }
    }

    /// θ: a loop's state-typed carried port is the θ over the state the loop was
    /// entered with and the one each edge back into the port carries — the body's
    /// latch, and every `break`/`continue` leaving its scope. The port's argument
    /// anchors first, so the regions can be read on it, and the θ joins that class
    /// after — an edge is a term over the argument itself. The loop's result is
    /// the port read where the loop was left.
    ///
    /// Only state ports: a value the loop carries is one the term graph already
    /// reads as an argument, and nothing yet asks it what the loop does to it.
    fn seed_theta(&mut self, instance: &OpHandle, loop_like: &dyn LoopLike) {
        let carried = loop_like.carried_args();
        let ports: Vec<usize> = carried
            .iter()
            .enumerate()
            .filter(|&(_, &argument)| self.context.get_value(argument).ty() == self.state_ty)
            .map(|(index, _)| index)
            .collect();
        let tested = tested_ports(self.context, instance, carried.len());
        let heads = match &tested {
            Some((_, arguments, _)) => arguments.clone(),
            None => carried.clone(),
        };
        for &port in &ports {
            self.anchor(heads[port]);
        }
        // The body reads what the test forwards into it, so the test's region is
        // read first and its forwarded values name the body's arguments.
        if let Some((region, _, forwarded)) = tested.clone() {
            self.seed_region(region);
            for &port in &ports {
                let id = self.class_of(forwarded[port]);
                self.value_class.insert(carried[port], id);
            }
        }
        for region in instance.regions().to_vec() {
            self.seed_region(region);
        }
        let (inits, finals) = (loop_like.inits(), loop_like.finals());
        let edges: Vec<OpHandle> = instance
            .clone()
            .as_interface::<dyn TokenScope>()
            .into_iter()
            .flat_map(|scope| scope.token_scope_regions())
            .flat_map(|body| port_edges(self.context, body))
            .map(|edge| self.context.get_op(edge))
            .collect();
        if edges.is_empty() {
            return;
        }
        for (slot, &port) in ports.iter().enumerate() {
            let init = self.class_of(inits[port]);
            let Some(states) = self.carried_states(&edges, slot, ports.len()) else {
                continue;
            };
            let head = self.class_of(heads[port]);
            let carried: Vec<Id> = states.iter().map(|&state| self.class_of(state)).collect();
            // A port every edge carries unchanged has the state the loop was
            // entered with for its fixpoint: θ(init, arg…) is init.
            if carried
                .iter()
                .all(|&edge| self.eg.find(edge) == self.eg.find(head))
            {
                self.eg.union(head, init);
            } else {
                let mut args = vec![init];
                args.extend(carried);
                let theta = self.eg.add(Node::theta(finals[port], args));
                self.eg.union(theta, head);
            }
            let published = match &tested {
                Some((_, _, forwarded)) => self.class_of(forwarded[port]),
                None => head,
            };
            self.value_class.insert(finals[port], published);
        }
        self.eg.rebuild();
    }

    /// The state each edge carries into the `slot`-th of a loop's `states` state
    /// ports: the ports are the trailing values an edge carries, in the loop's own
    /// order. `None` where an edge carries too few to name them.
    fn carried_states(
        &self,
        edges: &[OpHandle],
        slot: usize,
        states: usize,
    ) -> Option<Vec<ValueId>> {
        edges
            .iter()
            .map(|edge| {
                let carried = carried_operands(edge);
                let first = carried.len().checked_sub(states)?;
                carried.get(first + slot).copied()
            })
            .collect()
    }

    /// Record the states `instance` observes as exported. The term graph models a
    /// state only through the accesses on it, so a state any other operation takes
    /// — returned, yielded out of a region, handed to a call — is observed by
    /// something the graph cannot see, and no law may drop the write that left it.
    fn export_states(&mut self, instance: &OpHandle) {
        for operand in instance.operands().to_vec() {
            if self.context.get_value(operand).ty() == self.state_ty {
                let id = self.class_of(operand);
                self.exported_states.push(id);
            }
        }
    }

    /// Seed a memory access over the state it reads, if it is one that names a state.
    fn seed_memory(&mut self, instance: &OpHandle) -> bool {
        if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() {
            return self.seed_read(read.as_ref());
        }
        if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>() {
            return self.seed_write(instance, write.as_ref());
        }
        false
    }

    /// A read leaves memory as it found it, so the state it publishes is the one
    /// it read: both name the same class, and two reads of one address in one
    /// state are one term.
    fn seed_read(&mut self, read: &dyn MemoryRead) -> bool {
        let value = read.read_value();
        let ty = self.context.get_value(value).ty();
        let (Some(bits), Some(state)) = (self.access_width(ty), read.state_operand()) else {
            return false;
        };
        let address = self.class_of(read.read_location());
        let bytes = self.int(bits / 8);
        let metadata = self.int(0);
        let state = self.class_of(state);
        let id = self.eg.add(
            Node::access(
                SymKind::LoadMemory,
                value,
                vec![address, bytes, metadata, state],
            )
            .typed(ty),
        );
        self.value_class.insert(value, id);
        if let Some(published) = read.state_result() {
            self.value_class.insert(published, state);
        }
        true
    }

    /// A write's term *is* the state the accesses after it read. The term names
    /// the location, the value and the state, so a write carrying any other
    /// operand — a size, say — covers an extent the term cannot spell and is no
    /// store term at all.
    fn seed_write(&mut self, instance: &OpHandle, write: &dyn MemoryWrite) -> bool {
        if instance.operands().len() != STORE_OPERANDS {
            return false;
        }
        let written = write.written_value();
        let ty = self.context.get_value(written).ty();
        let (Some(bits), Some(state), Some(published)) = (
            self.access_width(ty),
            write.state_operand(),
            write.state_result(),
        ) else {
            return false;
        };
        let address = self.class_of(write.write_location());
        let bytes = self.int(bits / 8);
        let value = self.class_of(written);
        let address_space = self.int(0);
        let state = self.class_of(state);
        let id = self.eg.add(Node::access(
            SymKind::StoreMemory,
            published,
            vec![address, bytes, value, address_space, state],
        ));
        self.value_class.insert(published, id);
        true
    }

    /// The extent an access of `ty` covers, in bits. A pointer's width is a data
    /// layout fact, so where no layout is in scope the access covers an extent
    /// the term cannot spell and stays unseeded.
    fn access_width(&self, ty: TypeId) -> Option<u32> {
        type_width(self.context, ty).or_else(|| {
            let data = self.context.get_type_data(ty);
            (data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<crate::ptr::PtrType>()
                .and(self.pointer_width)
        })
    }

    /// A byte count or metadata literal, spelled the one way the vocabulary shares.
    fn int(&mut self, value: u32) -> Id {
        self.eg.add(Node::constant(
            minimal_unsigned_apint(u64::from(value)),
            Prov::None,
        ))
    }
}

/// A pure value op the e-graph may reason about: one result, no regions, and a declared semantic expression.
fn is_pure_value(instance: &OpHandle) -> bool {
    instance.results().len() == 1
        && instance.regions().is_empty()
        && instance
            .clone()
            .as_dyn_op()
            .semantic_expr(&mut crate::sem::SemGraph::new())
            .is_some()
}
