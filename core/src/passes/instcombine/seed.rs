//! Seeds the e-graph by reading a function's regions. Gates come off the ops'
//! own interfaces, never from control-flow analysis: a [`crate::Gamma`] result
//! is the γ over the values its arms name, and an arm's ports are the inputs
//! forwarded into it. A memory access is the `LoadMemory`/`StoreMemory`
//! term over the state it reads, so the chain is an ordinary edge. Everything
//! else the vocabulary cannot spell — a loop's carried port and results, a
//! multi-result or effectful op — anchors as an input leaf.

use std::collections::{HashMap, HashSet};

use tir_relational::{ClassId as Id, Engine};

use crate::analysis::effects::{observed_state, produced_state};
use crate::sem::egraph::{minimal_unsigned_apint, type_width};
use crate::sem::{Prov, SemNode as Node, SymKind};
use crate::state::{JoinOp, SplitOp};
use crate::{
    Commutative, ConstantLike, Context, Gamma, MemoryRead, MemoryWrite, OpHandle, OpId, RegionId,
    Theta, TypeId, ValueId,
};

/// The operands a store term names: the location, the value it writes, and the
/// state it observes.
const STORE_OPERANDS: usize = 3;

/// The seeded e-graph plus the driver's maps: each value's class, each block
/// argument's block, the state classes something outside the term graph
/// observes, and the value ports each loop carries.
pub struct Seeded {
    pub eg: Engine<Node>,
    pub value_class: HashMap<ValueId, Id>,
    pub loop_ports: Vec<LoopPorts>,
}

/// The value ports one loop carries, in the order it carries them.
pub struct LoopPorts {
    pub op: OpId,
    pub ports: Vec<Port>,
}

/// One carried value port: the class it is read as at the head of an iteration,
/// the class the loop is entered on, the class each edge back into it carries,
/// the class of the loop's result, and the class that result *is* — what the
/// loop publishes where it was left, which is the head itself unless a test
/// forwards something else.
pub struct Port {
    pub head: Id,
    pub init: Id,
    pub edges: Vec<Id>,
    pub result: Id,
    pub published: Id,
}

/// Build the e-graph for the regions of `root`.
pub fn seed(context: &Context, root: OpId) -> Seeded {
    let mut seeder = Seeder {
        context,
        eg: Engine::new(),
        value_class: HashMap::new(),
        seeded: HashSet::new(),
        pointer_width: crate::DataLayout::for_op(context, root)
            .and_then(|layout| layout.pointer_size()),
        loop_ports: Vec::new(),
    };
    for region in context.get_op(root).regions().to_vec() {
        seeder.seed_region(region);
    }
    Seeded {
        eg: seeder.eg,
        value_class: seeder.value_class,
        loop_ports: seeder.loop_ports,
    }
}

struct Seeder<'a> {
    context: &'a Context,
    eg: Engine<Node>,
    value_class: HashMap<ValueId, Id>,
    seeded: HashSet<OpId>,
    pointer_width: Option<u32>,
    loop_ports: Vec<LoopPorts>,
}

impl Seeder<'_> {
    fn seed_region(&mut self, region: RegionId) {
        let handle = self.context.get_region(region);
        for port in handle.ports() {
            self.class_of(port.id());
        }
        for op in handle.op_ids() {
            self.seed_op(op);
        }
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

    /// Record that `id` is the class of `value`, and that its terms carry the
    /// value's type — a term standing for an IR value keeps no type of its own,
    /// and the value has one.
    fn bind_value(&mut self, value: ValueId, id: Id) {
        let ty = self.context.get_value(value).ty();
        self.eg.raise_type(id, ty.number() as u64);
        self.value_class.insert(value, id);
    }

    /// The leaf standing for `value`, unless something already seeded a class for it.
    fn anchor(&mut self, value: ValueId) -> Id {
        if let Some(&id) = self.value_class.get(&value) {
            return id;
        }
        let id = self.eg.add(Node::input(value));
        self.bind_value(value, id);
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
            if let Some(gamma) = instance.clone().as_interface::<dyn Gamma>() {
                self.seed_switch(&instance, gamma.as_ref());
                return;
            }
            if let Some(theta) = instance.clone().as_interface::<dyn Theta>() {
                self.seed_loop(&instance, theta.as_ref());
                return;
            }
            // An operation the vocabulary cannot spell carries no reasoning:
            // its results anchor, but its regions are read like any other.
            for region in instance.regions().to_vec() {
                self.seed_region(region);
            }
        } else if let (Some(constant), [result]) = (
            instance.clone().as_interface::<dyn ConstantLike>(),
            instance.results().as_slice(),
        ) {
            let ty = self.context.get_value(*result).ty();
            let id = self
                .eg
                .add(Node::constant(constant.constant_value(), Prov::Op(op)).typed(ty));
            self.bind_value(*result, id);
            return;
        } else if instance.is::<JoinOp>() {
            self.seed_join(&instance);
            return;
        } else if instance.is::<SplitOp>() {
            // A split renames one memory per chain crossing it; each name is
            // the memory the state it takes already stands for, so a read of
            // any of them has observed what that state's writes left.
            let state = self.class_of(instance.state_operands()[0]);
            for result in instance.state_results() {
                self.bind_value(result, state);
            }
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
            self.bind_value(value, id);
            return;
        }

        for result in instance.results().to_vec() {
            self.anchor(result);
        }
    }

    /// A declared γ over unordered arms: every arm reads the forwarded inputs
    /// and the op's dependencies through its ports, and result `index` is the
    /// choice among the arms' `index`-th results by the predicate. Two arms
    /// picked by a boolean are the `If` the rules know, the true arm first;
    /// arms that agree need no choice at all.
    fn seed_switch(&mut self, instance: &OpHandle, gamma: &dyn Gamma) {
        let predicate = self.class_of(gamma.predicate());
        let binding = gamma.binding();
        let inputs = instance.operands()[binding.operands.clone()].to_vec();
        let arms = gamma.arms();
        for &arm in &arms {
            let region = self.context.get_region(arm);
            let ports = region.ports();
            for (port, &input) in ports[binding.ports.clone()].iter().zip(&inputs) {
                let id = self.class_of(input);
                self.bind_value(port.id(), id);
            }
            self.seed_region(arm);
        }
        let results = instance.results()[binding.results.clone()].to_vec();
        let boolean =
            type_width(self.context, self.context.get_value(gamma.predicate()).ty()) == Some(1);
        for (index, &result) in results.iter().enumerate() {
            if self.context.get_value(result).is_state() {
                continue;
            }
            let produced: Vec<Id> = arms
                .iter()
                .map(|&arm| {
                    let value = self.context.get_region(arm).results()[binding.exit.start + index];
                    self.class_of(value)
                })
                .collect();
            let first = self.eg.find(produced[0]);
            let id = if produced.iter().all(|&arm| self.eg.find(arm) == first) {
                produced[0]
            } else if boolean && produced.len() == 2 {
                self.eg.add(Node::gamma(
                    result,
                    vec![predicate, produced[1], produced[0]],
                ))
            } else {
                let mut args = vec![predicate];
                args.extend(produced);
                self.eg.add(Node::gamma(result, args))
            };
            self.bind_value(result, id);
        }
        for dep in instance.state_results() {
            self.anchor(dep);
        }
    }

    /// A chain a loop body hands back untouched — the port itself is what it
    /// names in both its continue and its exit group — is the memory the loop
    /// was entered on: nothing under the loop changes an object on that chain,
    /// on any iteration. So a read of it inside the body has observed what the
    /// state before the loop stands for, and so has one after the loop.
    fn bind_unchanged_chains(&mut self, instance: &OpHandle) {
        for chain in crate::binding::state_chains(self.context, instance) {
            let port = chain.ports[0];
            let continued = chain.next.expect("a loop carries a state on");
            if !names_same_memory(self.context, continued, port)
                || !names_same_memory(self.context, chain.exits[0], port)
            {
                continue;
            }
            let before = self.class_of(chain.entered);
            self.bind_value(port, before);
            self.bind_value(chain.left, before);
        }
    }

    /// A declared θ over an unordered body: each carried port is read inside
    /// the body as a `Port` of its own identity, and the value the loop produces
    /// for it is `Loop(init, next, exit, pred)`. A port every iteration carries
    /// unchanged is the value the loop was entered on, and the loop leaves with
    /// its exit value; any other value port is recorded for the hypothesis
    /// rounds. Dependencies anchor.
    fn seed_loop(&mut self, instance: &OpHandle, theta: &dyn Theta) {
        let binding = theta.binding();
        let body = theta.body();
        let region = self.context.get_region(body);
        let inits = instance.operands()[binding.operands.clone()].to_vec();
        let heads: Vec<ValueId> = region.ports()[binding.ports.clone()]
            .iter()
            .map(crate::Value::id)
            .collect();
        for &head in &heads {
            if !self.context.get_value(head).is_state() {
                self.anchor(head);
            }
        }
        self.bind_unchanged_chains(instance);
        self.seed_region(body);
        let predicate = self.class_of(theta.predicate());
        let body_results = region.results();
        let finals = instance.results()[binding.results.clone()].to_vec();
        let mut ports = Vec::new();
        for (index, &head_value) in heads.iter().enumerate() {
            if self.context.get_value(head_value).is_state() {
                continue;
            }
            let head = self.class_of(head_value);
            let init = self.class_of(inits[index]);
            let next = self.class_of(body_results[binding.continue_.start + index]);
            let exit = self.class_of(body_results[binding.exit.start + index]);
            let port = self.eg.add(Node::port(head_value, vec![head]));
            self.eg.union(port, head);
            let result = self.anchor(finals[index]);
            let node = self.eg.add(Node::loop_(
                finals[index],
                vec![init, next, exit, predicate],
            ));
            self.eg.union(node, result);
            if self.eg.find(next) == self.eg.find(head) {
                self.eg.union(head, init);
                self.eg.union(result, exit);
            } else {
                ports.push(Port {
                    head,
                    init,
                    edges: vec![next],
                    result,
                    published: exit,
                });
            }
        }
        for dep in instance.state_results() {
            self.anchor(dep);
        }
        if !ports.is_empty() {
            self.loop_ports.push(LoopPorts {
                op: instance.id,
                ports,
            });
        }
        self.eg.rebuild();
    }

    /// A join names the memory its inputs merge into, so its identity is the tuple
    /// of them: a read after a join has observed the writes before every input and
    /// is the read of no single one. Where the inputs are one state — the fork of
    /// reads a single write left, since a read publishes the state it took — the
    /// merge is that state.
    fn seed_join(&mut self, instance: &OpHandle) {
        let args: Vec<Id> = instance
            .operands()
            .to_vec()
            .iter()
            .map(|&state| self.class_of(state))
            .collect();
        let result = instance.results()[0];
        let ty = self.context.get_value(result).ty();
        let id = match args.split_first() {
            Some((&first, rest)) if rest.iter().all(|&other| other == first) => first,
            _ => self.eg.add(Node::seeded(instance, ty, true, args)),
        };
        self.bind_value(result, id);
    }

    /// Seed a memory access over the state it reads, if it is one that names a state.
    fn seed_memory(&mut self, instance: &OpHandle) -> bool {
        if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() {
            return self.seed_read(instance, read.as_ref());
        }
        if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>() {
            return self.seed_write(instance, write.as_ref());
        }
        false
    }

    /// A read leaves memory as it found it, so the state it publishes is the one
    /// it read: both name the same class, and two reads of one address in one
    /// state are one term.
    fn seed_read(&mut self, instance: &OpHandle, read: &dyn MemoryRead) -> bool {
        let value = read.read_value();
        let ty = self.context.get_value(value).ty();
        let (Some(bits), Some(state)) = (self.access_width(ty), observed_state(instance)) else {
            return false;
        };
        let location = read.read_location();
        let state = self.class_of(state);
        if let Some(published) = produced_state(instance) {
            self.bind_value(published, state);
        }
        let address = self.class_of(location);
        let bytes = self.int(bits / 8);
        let metadata = self.int(0);
        let id = self.eg.add(
            Node::access(
                SymKind::LoadMemory,
                value,
                vec![address, bytes, metadata, state],
            )
            .typed(ty),
        );
        self.bind_value(value, id);
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
            observed_state(instance),
            produced_state(instance),
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
        self.bind_value(published, id);
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

/// Whether `state` names the memory `port` does: the reads between them left
/// it as they found it, and a merge of one chain's fork of them names the state
/// they all observed.
///
/// A fork's join reaches every read of it, so the walk is over a DAG and has to
/// remember what it has answered; without that a run of forks costs one path
/// per read of each.
fn names_same_memory(context: &Context, state: ValueId, port: ValueId) -> bool {
    names_same_memory_seen(context, state, port, &mut HashSet::new())
}

fn names_same_memory_seen(
    context: &Context,
    state: ValueId,
    port: ValueId,
    seen: &mut HashSet<ValueId>,
) -> bool {
    if state == port {
        return true;
    }
    if !seen.insert(state) {
        return false;
    }
    let Some(op) = context.get_value(state).defining_op() else {
        return false;
    };
    let instance = context.get_op(op);
    if instance.has_interface::<dyn MemoryRead>()
        && !instance.has_interface::<dyn MemoryWrite>()
        && produced_state(&instance) == Some(state)
        && let Some(taken) = observed_state(&instance)
    {
        return names_same_memory_seen(context, taken, port, seen);
    }
    instance.is::<JoinOp>()
        && instance.state_results().as_slice() == [state]
        && instance
            .state_operands()
            .iter()
            .all(|&input| names_same_memory_seen(context, input, port, seen))
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
