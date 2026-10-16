//! Seeds the e-graph by reading a function's regions. Gates come off the ops'
//! own interfaces, never from control-flow analysis: a [`crate::Gamma`] result
//! is the γ over the values its arms name, and an arm's ports are the inputs
//! forwarded into it. A memory access is the `LoadMemory`/`StoreMemory`
//! term over the state it reads, so the chain is an ordinary edge. Everything
//! else the vocabulary cannot spell — a loop's carried port and results, a
//! multi-result or effectful op — anchors as an input leaf, or, for a client
//! that orders effects, as the row of what its op reads.

use crate::{IdMap, IdSet};

use smallvec::SmallVec;
use tir_egraph::{Carrier, Engine, LabelId, Ref, Vocabulary};

use crate::analysis::effects::{Effect, effect_of, observed_state, produced_state};
use crate::attributes::{AttributeValue, NamedAttribute};
use crate::builtin::FloatType;
use crate::operation::ValueIds;
use crate::sem::egraph::{LANES_WIDTH, carrier_of, counted_vector, minimal_unsigned_apint};
use crate::sem::node::WIDEST_RESULT;
use crate::sem::{IrOp, Prov, SemNode as Node, SymKind};
use crate::state::{JoinOp, SplitOp};
use crate::store::Db;
use crate::{
    Congruent, ConstantLike, Context, Gamma, HasResourceSemantics, MemoryRead, MemoryWrite,
    OpHandle, OpId, RegionId, ResourceAccess, ResourceEffects, Speculatable, Theta, TypeId,
    ValueId,
};
use tir_adt::{APFloat, APInt};

/// The operands a store term names: the location, the value it writes, and the
/// state it observes.
const STORE_OPERANDS: usize = 3;

const CONSTANT: u8 = 1;
const JOIN: u8 = 2;
const SPLIT: u8 = 4;
const ACCESS: u8 = 8;

// Read at the first op of a kind: purity depends only on the attributes and
// the function's data layout.
#[derive(Clone, Default)]
struct Kind {
    interfaces: Option<u8>,
    operators: Vec<(Vec<NamedAttribute>, Option<IrOp>)>,
}

/// What seeding learned of the op kinds it met, by name id, which a pass
/// keeps beside its vocabulary for its next function. It holds under one
/// data layout: a function under another starts it again.
#[derive(Clone, Default)]
pub struct Kinds {
    layout: Option<AttributeValue>,
    kinds: Vec<Kind>,
    unflagged: bool,
}

impl Kinds {
    /// Kinds whose operators carry no flags: selection has no poison to
    /// keep, so a flagged op selects as the unflagged one does.
    pub fn unflagged() -> Self {
        Self {
            unflagged: true,
            ..Self::default()
        }
    }

    // What holds of the kinds under `layout`: every interface, and the
    // operators of the pure ops. An op that is not pure may name what only
    // its function has, a symbol say, so those are met again.
    fn under(&mut self, layout: Option<AttributeValue>) -> &mut Vec<Kind> {
        if self.layout != layout {
            *self = Self {
                layout,
                kinds: Vec::new(),
                unflagged: self.unflagged,
            };
        }
        for kind in &mut self.kinds {
            kind.operators.retain(|(_, operator)| operator.is_some());
        }
        &mut self.kinds
    }
}

// The pure ops of one label whose classes are reserved and whose rows are
// yet to be stored, in walk order: an operand column per position. A batch
// stays, empty, once stored.
struct Batch {
    label: LabelId,
    operands: Vec<Vec<Ref>>,
    values: Vec<Ref>,
    nodes: Vec<Node>,
}

/// The seeded e-graph plus the driver's maps: each value's reference, the value
/// ports each loop carries, and the pointer width the carriers were read at.
pub struct Seeded {
    pub eg: Engine<Node>,
    pub value_class: IdMap<ValueId, Ref>,
    pub loop_ports: Vec<LoopPorts>,
    pub pointer_width: Option<u32>,
}

/// The value ports one loop carries, in the order it carries them.
pub struct LoopPorts {
    pub op: OpId,
    pub ports: Vec<Port>,
}

/// One carried value port: the value it is read as at the head of an
/// iteration, the value the loop is entered on, the value each edge back into
/// it carries, the loop's result, and the value that result *is* — what the
/// loop publishes where it was left, which is the head itself unless a test
/// forwards something else. A counter's next value is the head at offset one,
/// which is not the head.
pub struct Port {
    pub head: Ref,
    pub init: Ref,
    pub edges: Vec<Ref>,
    pub result: Ref,
    pub published: Ref,
}

/// Build the e-graph for the regions of `root`, its labels `vocabulary`'s,
/// reading what `kinds` holds of the op kinds and adding to it.
///
/// With `dependencies`, a result the vocabulary cannot spell is no leaf but
/// the row of what its op reads ([`crate::sem::Kind::Result`]), and a value a
/// gate or a loop chooses has that row beside its term: the graph then holds
/// what every value waits for.
pub fn seed(
    context: &Context,
    root: OpId,
    vocabulary: Vocabulary<Node>,
    kinds: &mut Kinds,
    dependencies: bool,
) -> Seeded {
    seed_with(context, root, vocabulary, kinds, dependencies, |seeder| {
        for region in context.get_op(root).regions().to_vec() {
            seeder.seed_region(region);
        }
    })
}

/// Build the e-graph of only `ops`, ops under `root`, each over leaves for
/// the values it reads: all a lowering whose rules each match one op over
/// its operands asks of a function.
pub fn seed_ops(
    context: &Context,
    root: OpId,
    ops: &[OpId],
    vocabulary: Vocabulary<Node>,
    kinds: &mut Kinds,
) -> Seeded {
    seed_with(context, root, vocabulary, kinds, false, |seeder| {
        for &op in ops {
            for operand in context.get_op(op).operands() {
                seeder.anchor(operand);
            }
            seeder.seed_op(op);
        }
    })
}

fn seed_with(
    context: &Context,
    root: OpId,
    vocabulary: Vocabulary<Node>,
    kinds: &mut Kinds,
    dependencies: bool,
    walk: impl FnOnce(&mut Seeder<'_>),
) -> Seeded {
    let layout = crate::DataLayout::for_op(context, root);
    let pointer_width = layout.as_ref().and_then(crate::DataLayout::pointer_size);
    let unflagged = kinds.unflagged;
    let mut seeder = Seeder {
        context,
        root,
        dependencies,
        eg: Engine::over(vocabulary),
        value_class: IdMap::new(),
        seeded: IdSet::new(),
        kinds: kinds.under(layout.map(crate::DataLayout::into_value)),
        pointer_width,
        unflagged,
        loop_ports: Vec::new(),
        pending: Vec::new(),
        queued: Vec::new(),
        unsettled: false,
    };
    walk(&mut seeder);
    seeder.settle();
    Seeded {
        eg: seeder.eg,
        value_class: seeder.value_class,
        loop_ports: seeder.loop_ports,
        pointer_width: seeder.pointer_width,
    }
}

struct Seeder<'a> {
    context: &'a Context,
    root: OpId,
    dependencies: bool,
    eg: Engine<Node>,
    value_class: IdMap<ValueId, Ref>,
    seeded: IdSet<OpId>,
    pointer_width: Option<u32>,
    unflagged: bool,
    loop_ports: Vec<LoopPorts>,
    kinds: &'a mut Vec<Kind>,
    // The pure ops walked and not yet stored, a batch per label met, and the
    // batches that hold some, in the order they got their first.
    pending: Vec<Batch>,
    queued: Vec<usize>,
    // Whether a batch was stored since the graph was last rebuilt.
    unsettled: bool,
}

impl Seeder<'_> {
    fn seed_region(&mut self, region: RegionId) {
        let handle = self.context.get_region(region);
        for port in handle.ports() {
            self.class_of(port.id());
        }
        let ops = handle.op_ids();
        let context = self.context;
        let mut at = 0;
        while at < ops.len() {
            at += context.with_rows(|rows| self.seed_rows(rows, &ops[at..]));
            if let Some(&op) = ops.get(at) {
                self.seed_by_handle(op);
                at += 1;
            }
        }
    }

    // How many of the leading `ops` their rows alone seed. The op the count
    // stops at is marked seeded and needs its handle.
    fn seed_rows(&mut self, rows: &Db, ops: &[OpId]) -> usize {
        let by_row = |&&op: &&OpId| !self.seeded.insert(op) || self.seed_row(rows, op);
        ops.iter().take_while(by_row).count()
    }

    // Whether `op` is a pure op of an operator already met, with one result
    // and operands that have their classes, and is seeded now.
    fn seed_row(&mut self, rows: &Db, op: OpId) -> bool {
        let row = rows.op_cells(op);
        let mut results = row.results();
        let (Some(value), None, false) = (results.next(), results.next(), row.owns_regions())
        else {
            return false;
        };
        let Some(kind) = self.kinds.get(row.name().index()) else {
            return false;
        };
        if kind.interfaces != Some(0) {
            return false;
        }
        let mut operators = kind.operators.iter();
        let Some((_, Some(operator))) = operators.find(|(seen, _)| seen == row.attrs()) else {
            return false;
        };
        let operator = operator.clone();
        self.seed_pure(rows, op, operator, value)
    }

    // Reserve the class of the pure op `op`, which `value` stands for, and
    // queue its row over its value operands and then its states, unless an
    // operand has no class yet.
    fn seed_pure(&mut self, rows: &Db, op: OpId, operator: IrOp, value: ValueId) -> bool {
        let ty = rows.value_type(value);
        let mut args: SmallVec<[Ref; 4]> = SmallVec::new();
        let mut states: SmallVec<[Ref; 2]> = SmallVec::new();
        let mut result_typed = 0;
        let mut operands = rows.op_cells(op).operands();
        if !operands.all(|operand| self.value_class.contains_key(&operand))
            && !self.seed_definitions(rows, op)
        {
            return false;
        }
        for (index, operand) in rows.op_cells(op).operands().enumerate() {
            let class = self.value_class[&operand];
            let operand_ty = rows.value_type(operand);
            if operand_ty == ty {
                result_typed |= IrOp::operand_bit(index);
            }
            match self.context.is_state_type(operand_ty) {
                true => states.push(class),
                false => args.push(class),
            }
        }
        let carrier = carrier_of(self.context, self.pointer_width, ty);
        // The lane count of a vector type is no part of the label: the row
        // reads it where it would a length it is given.
        let counted = match carrier {
            None => counted_vector(self.context, ty),
            Some(_) => None,
        };
        let lanes = counted.map(|(_, lanes)| {
            let lanes = APInt::new(u64::from(LANES_WIDTH), u64::from(lanes));
            self.eg.add(Node::constant(lanes, Prov::None))
        });
        let args = row_children(args, lanes, states);
        let labelled = counted.map_or(ty, |(any, _)| any);
        let node =
            Node::seeded_row(operator, op, labelled, result_typed, classes(&args)).carried(carrier);
        let id = self.defer(node, &args);
        self.eg.raise_type(id.class, ty.number() as u64);
        self.value_class.insert(value, id);
        true
    }

    // Whether every operand of `op` has its class once the pure ops that
    // define the ones without are seeded off their rows: the values first
    // and then the states, each definition under its own operands.
    fn seed_definitions(&mut self, rows: &Db, op: OpId) -> bool {
        [false, true].into_iter().all(|states| {
            rows.op_cells(op).operands().all(|operand| {
                self.context.is_state_type(rows.value_type(operand)) != states
                    || self.seed_definition(rows, operand)
            })
        })
    }

    // Whether `value` has a class, seeding the op of the function that
    // defines it first where its row alone seeds it.
    fn seed_definition(&mut self, rows: &Db, value: ValueId) -> bool {
        if self.value_class.contains_key(&value) {
            return true;
        }
        let Some(op) = rows.value_def(value) else {
            return false;
        };
        let mut holders = std::iter::successors(rows.enclosing_op_of(op), |&holder| {
            rows.enclosing_op_of(holder)
        });
        if !holders.any(|holder| holder == self.root) || !self.seeded.insert(op) {
            return false;
        }
        let seeded = self.seed_row(rows, op);
        if !seeded {
            // Its handle seeds it where the walk meets it.
            self.seeded.remove(&op);
        }
        seeded
    }

    // A pure op gets its class where the walk meets it, so classes are in
    // walk order, and its row when its label's batch is stored.
    fn defer(&mut self, node: Node, args: &[Ref]) -> Ref {
        let (value, label) = self.eg.reserve_class(&node, args);
        let Some(label) = label else {
            return value;
        };
        let at = self.pending.iter().position(|batch| batch.label == label);
        let at = at.unwrap_or_else(|| {
            self.pending.push(Batch {
                label,
                operands: vec![Vec::new(); args.len()],
                values: Vec::new(),
                nodes: Vec::new(),
            });
            self.pending.len() - 1
        });
        let batch = &mut self.pending[at];
        if batch.values.is_empty() {
            self.queued.push(at);
        }
        for (column, &arg) in batch.operands.iter_mut().zip(args) {
            column.push(arg);
        }
        batch.values.push(value);
        batch.nodes.push(node);
        value
    }

    fn flush(&mut self) {
        for at in self.queued.drain(..) {
            let batch = &mut self.pending[at];
            {
                let operands: SmallVec<[&[Ref]; 4]> =
                    batch.operands.iter().map(Vec::as_slice).collect();
                let nodes = batch.nodes.drain(..);
                self.eg
                    .insert_rows(batch.label, &operands, &batch.values, nodes);
            }
            batch.operands.iter_mut().for_each(Vec::clear);
            batch.values.clear();
            self.unsettled = true;
        }
    }

    // What a one-at-a-time insert would know of the walked ops by now: a
    // read of the partition comes after this.
    fn settle(&mut self) {
        self.flush();
        if std::mem::take(&mut self.unsettled) {
            self.eg.rebuild_seeded();
        }
    }

    fn class_of(&mut self, value: ValueId) -> Ref {
        if let Some(&id) = self.value_class.get(&value) {
            return id;
        }
        if let Some(op) = self.context.get_value(value).defining_op() {
            let mut holders = std::iter::successors(self.context.parent_op(op), |&holder| {
                self.context.parent_op(holder)
            });
            if !holders.any(|holder| holder == self.root) {
                return self.opaque(value, 0, false, self.dependencies.then_some(&[]));
            }
            self.seed_op(op);
        }
        self.anchor(value)
    }

    fn bind_value(&mut self, value: ValueId, id: Ref) {
        let ty = self.context.get_value(value).ty();
        self.eg.raise_type(id.class, ty.number() as u64);
        self.value_class.insert(value, id);
    }

    fn carrier(&self, value: ValueId) -> Option<Carrier> {
        carrier_of(
            self.context,
            self.pointer_width,
            self.context.get_value(value).ty(),
        )
    }

    fn anchor(&mut self, value: ValueId) -> Ref {
        if let Some(&id) = self.value_class.get(&value) {
            return id;
        }
        let id = self.eg.add(Node::input(value).carried(self.carrier(value)));
        self.bind_value(value, id);
        id
    }

    // `read` as the children of one row. Where a row does not hold them all
    // they are rows of their own, as deep as it takes.
    fn bundled(&mut self, mut read: Vec<Ref>) -> Vec<Ref> {
        while read.len() > WIDEST_RESULT {
            let rows = read.chunks(WIDEST_RESULT);
            let rows = rows.map(|held| self.eg.insert(Node::operands(classes(held)), held));
            read = rows.collect();
        }
        read
    }

    fn read(&mut self, values: &[ValueId]) -> Vec<Ref> {
        let read = values.iter().map(|&value| self.class_of(value)).collect();
        self.bundled(read)
    }

    // What `instance` reads, where the client asked for the rows of it: its
    // operands, then what its regions yield.
    fn read_by(&mut self, instance: &OpHandle) -> Option<Vec<Ref>> {
        let mut values = instance.operands().to_vec();
        for &region in instance.regions().iter() {
            values.extend(self.context.get_region(region).results());
        }
        self.dependencies.then(|| self.read(&values))
    }

    // The class of `value`, value `index` of an op the vocabulary cannot
    // spell, unless something already seeded one: the row over what the op
    // reads, `read`, or a leaf where the client asked for none.
    fn opaque(&mut self, value: ValueId, index: usize, changes: bool, read: Option<&[Ref]>) -> Ref {
        if let Some(&id) = self.value_class.get(&value) {
            return id;
        }
        let Some(read) = read else {
            return self.anchor(value);
        };
        let node = Node::result(value, index, changes, classes(read));
        let id = self.eg.insert(node.carried(self.carrier(value)), read);
        self.bind_value(value, id);
        id
    }

    // [`Self::opaque`] for each of `results`, which `instance` defines.
    fn opaque_results(
        &mut self,
        instance: &OpHandle,
        results: &[ValueId],
        changes: bool,
        read: Option<&[Ref]>,
    ) {
        for (index, result) in instance.results().to_vec().into_iter().enumerate() {
            if results.contains(&result) {
                self.opaque(result, index, changes, read);
            }
        }
    }

    // Say beside the term of `value`, result `index` of its op, that it
    // waits for `read`: all a gate or a loop reads, what the arms or the
    // body wrote among it, or the states a merge too wide for a rule merges.
    fn chosen(&mut self, value: ValueId, index: usize, changes: bool, read: &[Ref]) {
        let ty = self.context.get_value(value).ty();
        let node = Node::chosen(value, index, changes, classes(read)).typed(ty);
        let row = self.eg.insert(node.carried(self.carrier(value)), read);
        let _ = self.eg.union(self.value_class[&value], row);
        self.unsettled = true;
    }

    fn kind(&mut self, instance: &OpHandle) -> &mut Kind {
        let kind = self.context.op_name_id(instance.id).index();
        if self.kinds.len() <= kind {
            self.kinds.resize_with(kind + 1, Kind::default);
        }
        &mut self.kinds[kind]
    }

    fn interfaces(&mut self, instance: &OpHandle) -> u8 {
        if let Some(interfaces) = self.kind(instance).interfaces {
            return interfaces;
        }
        let access = instance.has_interface::<dyn MemoryRead>()
            || instance.has_interface::<dyn MemoryWrite>();
        let interfaces = [
            (instance.has_interface::<dyn ConstantLike>(), CONSTANT),
            (instance.is::<JoinOp>(), JOIN),
            (instance.is::<SplitOp>(), SPLIT),
            (access, ACCESS),
        ]
        .iter()
        .fold(0, |bits, &(has, bit)| bits | if has { bit } else { 0 });
        self.kind(instance).interfaces = Some(interfaces);
        interfaces
    }

    fn pure(&mut self, instance: &OpHandle) -> Option<(IrOp, ValueIds)> {
        let values = instance.value_results();
        if values.len() > 1 || !instance.regions().is_empty() {
            return None;
        }
        let context = self.context;
        let unflagged = self.unflagged;
        let kind = self.kind(instance);
        let known = context.with_attributes(instance.id, |attributes| {
            let mut operators = kind.operators.iter();
            operators
                .find(|(seen, _)| seen == attributes)
                .map(|(_, operator)| operator.clone())
        });
        let operator = known.unwrap_or_else(|| {
            let operator = is_pure_value(instance).then(|| match unflagged {
                true => IrOp::of(instance).without_flags(),
                false => IrOp::of(instance),
            });
            kind.operators
                .push((instance.attributes(), operator.clone()));
            operator
        });
        Some((operator?, values))
    }

    fn seed_op(&mut self, op: OpId) {
        if self.seeded.insert(op) {
            self.seed_by_handle(op);
        }
    }

    fn seed_by_handle(&mut self, op: OpId) {
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
        } else if let (true, [result]) = (
            self.interfaces(&instance) & CONSTANT != 0,
            instance.results().as_slice(),
        ) {
            let constant = instance
                .clone()
                .as_interface::<dyn ConstantLike>()
                .expect("the kind is constant-like");
            let ty = self.context.get_value(*result).ty();
            let value = constant.constant_value();
            let node = match FloatType::view(&self.context, ty) {
                Some(float) => {
                    let (exponent, mantissa) = (float.exp_width(), float.mant_width());
                    let bits = u128::from(value.to_u64());
                    Node::float(
                        APFloat::from_bits(exponent, mantissa, false, bits),
                        Prov::Op(op),
                    )
                }
                None => Node::constant(value, Prov::Op(op)).carried(carrier_of(
                    self.context,
                    self.pointer_width,
                    ty,
                )),
            };
            let id = self.eg.add(node.typed(ty));
            self.bind_value(*result, id);
            return;
        } else if self.interfaces(&instance) & JOIN != 0 {
            self.seed_join(&instance);
            return;
        } else if self.interfaces(&instance) & SPLIT != 0 {
            // A split renames one memory per chain crossing it; each name is
            // the memory the state it takes already stands for, so a read of
            // any of them has observed what that state's writes left.
            let state = self.class_of(instance.state_operands()[0]);
            for result in instance.state_results() {
                self.bind_value(result, state);
            }
            return;
        } else if self.interfaces(&instance) & ACCESS != 0 && self.seed_memory(&instance) {
            return;
        } else if let Some((operator, values)) = self.pure(&instance) {
            let published = instance.state_results();
            let value = *values
                .first()
                .or(published.first())
                .expect("a pure op defines a value or a state");
            // An op on an environment names the state it observes after its
            // values. One that only reads the environment, a floating-point
            // one rounding as the mode says, leaves it as it found it: the
            // state it publishes is the one it observed, of the same kind.
            // One that changes it, raising a flag, publishes a state of its
            // own, so what reads the flags after it is no read before it.
            let states = instance.state_operands();
            for &operand in instance.value_operands().iter().chain(&states) {
                self.class_of(operand);
            }
            let context = self.context;
            let seeded = context.with_rows(|rows| self.seed_pure(rows, op, operator, value));
            debug_assert!(seeded, "every operand has its class");
            let changed: Vec<ValueId> = instance
                .clone()
                .as_interface::<dyn ResourceEffects>()
                .map(|effects| effects.resource_effects())
                .unwrap_or_default()
                .into_iter()
                .filter(|effect| effect.access == ResourceAccess::Change)
                .flat_map(|effect| effect.produced)
                .collect();
            for published in published {
                if published == value {
                    continue;
                }
                if changed.contains(&published) {
                    let read = self.read_by(&instance);
                    self.opaque_results(&instance, &[published], true, read.as_deref());
                    continue;
                }
                let ty = self.context.get_value(published).ty();
                let observed = states
                    .iter()
                    .find(|&&state| self.context.get_value(state).ty() == ty);
                if let Some(&observed) = observed {
                    let class = self.class_of(observed);
                    self.bind_value(published, class);
                }
            }
            return;
        }

        let read = self.read_by(&instance);
        let changes = read.is_some() && changes(&instance);
        self.opaque_results(&instance, &instance.results(), changes, read.as_deref());
    }

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
        let boolean = self
            .context
            .terms()
            .bit_width(self.context.get_value(gamma.predicate()).ty())
            == Some(1);
        let mut chosen = Vec::new();
        for (index, &result) in results.iter().enumerate() {
            if self
                .context
                .is_state_type(self.context.get_value(result).ty())
            {
                continue;
            }
            let produced: Vec<Ref> = arms
                .iter()
                .map(|&arm| {
                    let value = self.context.get_region(arm).results()[binding.exit.start + index];
                    self.class_of(value)
                })
                .collect();
            self.settle();
            let first = self.eg.find(produced[0]);
            let id = if produced.iter().all(|&arm| self.eg.find(arm) == first) {
                produced[0]
            } else {
                let args = if boolean && produced.len() == 2 {
                    vec![predicate, produced[1], produced[0]]
                } else {
                    std::iter::once(predicate).chain(produced).collect()
                };
                let node = Node::gamma(result, classes(&args)).carried(self.carrier(result));
                chosen.push((binding.results.start + index, result));
                self.eg.insert(node, &args)
            };
            self.bind_value(result, id);
        }
        let read = self.read_by(instance);
        let changes = read.is_some() && changes(instance);
        for (index, result) in chosen.into_iter().filter(|_| read.is_some()) {
            self.chosen(result, index, changes, read.as_deref().unwrap_or_default());
        }
        self.opaque_results(
            instance,
            &instance.state_results(),
            changes,
            read.as_deref(),
        );
    }

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
            if !self
                .context
                .is_state_type(self.context.get_value(head).ty())
            {
                self.anchor(head);
            }
        }
        self.bind_unchanged_chains(instance);
        if self.dependencies {
            // A state the body changes is read at the head of an iteration
            // no earlier than the loop is entered.
            let entered = self.read(&instance.operands());
            for (index, &head) in heads.iter().enumerate() {
                self.opaque(head, binding.ports.start + index, false, Some(&entered));
            }
        }
        self.seed_region(body);
        let predicate = self.class_of(theta.predicate());
        let body_results = region.results();
        let finals = instance.results()[binding.results.clone()].to_vec();
        let mut ports = Vec::new();
        let mut chosen = Vec::new();
        for (index, &head_value) in heads.iter().enumerate() {
            if self
                .context
                .is_state_type(self.context.get_value(head_value).ty())
            {
                continue;
            }
            let head = self.class_of(head_value);
            let init = self.class_of(inits[index]);
            let next = self.class_of(body_results[binding.continue_.start + index]);
            let exit = self.class_of(body_results[binding.exit.start + index]);
            let carrier = self.carrier(head_value);
            let port = self.eg.insert(
                Node::port(head_value, classes(&[head])).carried(carrier),
                &[head],
            );
            let _ = self.eg.union(port, head);
            let result = self.anchor(finals[index]);
            let args = [init, next, exit, predicate];
            let node = self.eg.insert(
                Node::loop_(finals[index], classes(&args)).carried(carrier),
                &args,
            );
            let _ = self.eg.union(node, result);
            self.settle();
            if self.eg.connected(next, head) {
                let _ = self.eg.union(head, init);
                if self.eg.connected(exit, head) || self.eg.int_const(exit).is_some() {
                    let _ = self.eg.union(result, exit);
                    continue;
                }
            }
            chosen.push((binding.results.start + index, finals[index]));
            ports.push(Port {
                head,
                init,
                edges: vec![next],
                result,
                published: exit,
            });
        }
        let read = self.read_by(instance);
        let changes = read.is_some() && changes(instance);
        for (index, result) in chosen.into_iter().filter(|_| read.is_some()) {
            self.chosen(result, index, changes, read.as_deref().unwrap_or_default());
        }
        self.opaque_results(
            instance,
            &instance.state_results(),
            changes,
            read.as_deref(),
        );
        if !ports.is_empty() {
            self.loop_ports.push(LoopPorts {
                op: instance.id,
                ports,
            });
        }
        self.settle();
        self.eg.rebuild();
    }

    fn seed_join(&mut self, instance: &OpHandle) {
        let args: Vec<Ref> = instance
            .operands()
            .to_vec()
            .iter()
            .map(|&state| self.class_of(state))
            .collect();
        let result = instance.results()[0];
        let ty = self.context.get_value(result).ty();
        let same = |args: &[Ref]| args.iter().all(|&other| other == args[0]);
        let merged = args.is_empty() || !same(&args) && !self.settled_same(&args);
        let id = if merged {
            self.eg
                .insert(Node::seeded(instance, ty, true, classes(&args)), &args)
        } else {
            args[0]
        };
        self.bind_value(result, id);
        if merged && self.dependencies && args.len() > WIDEST_RESULT {
            let read = self.bundled(args);
            self.chosen(result, 0, false, &read);
        }
    }

    // Two inputs that are not one reference may be one state once the pure
    // ops walked so far are stored.
    fn settled_same(&mut self, args: &[Ref]) -> bool {
        self.settle();
        let first = self.eg.find(args[0]);
        args.iter().all(|&other| self.eg.find(other) == first)
    }

    fn seed_memory(&mut self, instance: &OpHandle) -> bool {
        if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() {
            return self.seed_read(instance, read.as_ref());
        }
        if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>() {
            return self.seed_write(instance, write.as_ref());
        }
        false
    }

    fn seed_read(&mut self, instance: &OpHandle, read: &dyn MemoryRead) -> bool {
        let value = read.read_value();
        let ty = self.context.get_value(value).ty();
        let (Some(bits), Some(state)) = (self.access_width(ty), observed_state(instance)) else {
            return false;
        };
        let state = self.class_of(state);
        if let Some(published) = produced_state(instance) {
            // A read that observes what this one left waits for this one,
            // which the state's own class would not say: for a client that
            // orders effects the state left is then a row over the state
            // observed, ordered as a write's is.
            match self.dependencies && reread(self.context, published, &mut IdSet::new()) {
                true => self.opaque_results(instance, &[published], true, Some(&[state])),
                false => self.bind_value(published, state),
            }
        }
        let args = self.spelled(instance, [bits / 8, 0]);
        let id = self.eg.insert(
            Node::access(SymKind::LoadMemory, value, classes(&args))
                .typed(ty)
                .carried(self.carrier(value)),
            &args,
        );
        self.bind_value(value, id);
        true
    }

    fn seed_write(&mut self, instance: &OpHandle, write: &dyn MemoryWrite) -> bool {
        if instance.operands().len() != STORE_OPERANDS {
            return false;
        }
        let written = write.written_value();
        let ty = self.context.get_value(written).ty();
        let (Some(bits), Some(_), Some(published)) = (
            self.access_width(ty),
            observed_state(instance),
            produced_state(instance),
        ) else {
            return false;
        };
        let args = self.spelled(instance, [bits / 8, 0]);
        let id = self.eg.insert(
            Node::access(SymKind::StoreMemory, published, classes(&args)),
            &args,
        );
        self.bind_value(published, id);
        true
    }

    fn access_width(&self, ty: TypeId) -> Option<u32> {
        self.context
            .terms()
            .bit_width(ty)
            .map(|width| width as u32)
            .or_else(|| crate::ptr::PtrType::view(&self.context, ty).and(self.pointer_width))
    }

    // The children of the access `instance` is seeded as: the class of each
    // operand [`children`] names, and the literals `own` where it names none.
    fn spelled(&mut self, instance: &OpHandle, own: [u32; 2]) -> SmallVec<[Ref; 5]> {
        let mut own = own.into_iter();
        let children = children(self.context, instance).into_iter();
        let spelled = children.map(|child| match child {
            Some(operand) => self.class_of(operand),
            None => self.int(own.next().expect("an access spells two children itself")),
        });
        spelled.collect()
    }

    fn int(&mut self, value: u32) -> Ref {
        self.eg.add(Node::constant(
            minimal_unsigned_apint(u64::from(value)),
            Prov::None,
        ))
    }
}

// The children of a pure op's row: its value operands, the lane count of a
// counted vector, its states.
fn row_children<T>(
    values: impl IntoIterator<Item = T>,
    lanes: Option<T>,
    states: impl IntoIterator<Item = T>,
) -> SmallVec<[T; 5]> {
    values.into_iter().chain(lanes).chain(states).collect()
}

pub(crate) fn children(context: &Context, instance: &OpHandle) -> SmallVec<[Option<ValueId>; 5]> {
    if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() {
        let state = observed_state(instance);
        return SmallVec::from_slice(&[Some(read.read_location()), None, None, state]);
    }
    if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>() {
        let (location, written) = (write.write_location(), write.written_value());
        let state = observed_state(instance);
        return SmallVec::from_slice(&[Some(location), None, Some(written), None, state]);
    }
    let defined = instance.value_results().first().copied();
    let ty = defined.map(|value| context.get_value(value).ty());
    let counted = ty.and_then(|ty| counted_vector(context, ty));
    row_children(
        instance.value_operands().into_iter().map(Some),
        counted.map(|_| None),
        instance.state_operands().into_iter().map(Some),
    )
}

fn classes(operands: &[Ref]) -> Vec<tir_egraph::ClassId> {
    operands.iter().map(|operand| operand.class).collect()
}

// Whether a read observes `state` before any write does: itself, or through
// the splits and the merges of it.
fn reread(context: &Context, state: ValueId, seen: &mut IdSet<ValueId>) -> bool {
    if !seen.insert(state) {
        return false;
    }
    context.users_of(state).into_iter().any(|user| {
        let user = context.get_op(user);
        match user.is::<SplitOp>() || user.is::<JoinOp>() {
            true => {
                let mut passed = user.state_results().into_iter();
                passed.any(|passed| reread(context, passed, seen))
            }
            false => effect_of(&user) == Some(Effect::Read),
        }
    })
}

fn names_same_memory(context: &Context, state: ValueId, port: ValueId) -> bool {
    names_same_memory_seen(context, state, port, &mut IdSet::new())
}

fn names_same_memory_seen(
    context: &Context,
    state: ValueId,
    port: ValueId,
    seen: &mut IdSet<ValueId>,
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

// Whether `instance` changes a state it publishes: what its effects declare,
// and any state at all where it declares none. A gate or a loop declares
// none, and the chain it publishes a state of hands it what the reads before
// it left, as it does a write.
fn changes(instance: &OpHandle) -> bool {
    match instance.clone().as_interface::<dyn ResourceEffects>() {
        Some(declared) => declared
            .resource_effects()
            .iter()
            .any(|effect| effect.access == ResourceAccess::Change && !effect.produced.is_empty()),
        None => !instance.state_results().is_empty(),
    }
}

fn is_pure_value(instance: &OpHandle) -> bool {
    let congruent = instance.has_interface::<dyn Congruent>();
    let effect = !instance.state_results().is_empty()
        && (congruent || instance.has_interface::<dyn HasResourceSemantics>());
    if instance.value_results().len() != 1 {
        return effect;
    }
    // A speculatable op over no operand, a symbol's address, stays a leaf of
    // its own: the commit cannot rebuild it where another region reads it.
    let speculatable = !instance.operands().is_empty()
        && instance
            .clone()
            .as_interface::<dyn Speculatable>()
            .is_some_and(|op| op.is_speculatable());
    effect
        || congruent
        || speculatable
        || instance
            .clone()
            .as_dyn_op()
            .semantic_expr(&mut crate::sem::SemGraph::new())
            .is_some()
}
