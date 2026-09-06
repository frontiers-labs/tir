//! Lowering of a function's IR operations into the semantic e-graph.

use std::collections::{HashMap, HashSet};

use tir::{
    Context, Gamma, MemoryRead, MemoryWrite, OpHandle, OpId, RegionId, Theta, TypeId, ValueId,
    attributes::AttributeValue,
    builtin::{FloatType, IntegerType},
    graph::{Dag, MetaDag, NodeId},
    sem::{
        SemGraph, SemNode, SemPayload, SemType, SymKind, SymPayload,
        egraph::{SemEGraph, ir_type, minimal_unsigned_apint, semantic_type, type_width},
        infer_types, template_node,
    },
};
use tir_adt::APInt;
use tir_relational::ClassId as Id;

use super::node::class_is_pure;

/// What a walk records for the cover: the class each operation is rooted at, and
/// the float constants a target materializer could build.
#[derive(Default)]
pub(crate) struct Seeds {
    pub(crate) roots_by_op: HashMap<OpId, Id>,
    pub(crate) constant_candidates: Vec<(OpId, Id)>,
}

/// Which test of a structured operation's destruction a class stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AuxSlot {
    /// The test selecting arm `k` of a gate, or a loop's repeat predicate
    /// (`k == 0`): taken when the class holds.
    Test(usize),
    /// The test selecting arm `k` of a gate, taken when the class does *not*
    /// hold: a one-bit predicate selects arm 0 by being false, and the class
    /// is the predicate itself, so the target's branch rules see the condition
    /// they were written against.
    Unless(usize),
}

/// The tests a destruction branches on, which no operation spells: a gate
/// selects an arm by its predicate's value, a loop repeats on a body result.
/// The seeder builds the terms so the cover selects them like any other, keyed
/// by the region whose plan must materialize them.
#[derive(Default)]
pub(crate) struct RegionControl {
    pub(crate) aux: HashMap<RegionId, Vec<(OpId, AuxSlot, Id)>>,
    /// Each (consumer, condition) pair a destruction's branch recomputes, so the
    /// consumer's use of it does not also force the condition into a register.
    pub(crate) test_conditions: HashSet<(OpId, ValueId)>,
}

impl RegionControl {
    fn record(&mut self, region: RegionId, op: OpId, slot: AuxSlot, class: Id) {
        self.aux.entry(region).or_default().push((op, slot, class));
    }
}

/// Builds a block's semantic expressions straight into the e-graph: every lowered
/// node is hash-consed by [`SemEngine::add`], so the e-graph *is* the interned DAG
/// (no separate arena). Returns e-class [`Id`]s and records, in `value_to_class`,
/// the class built for each IR value so operands share and cross-block uses expand.
pub(crate) struct SemDagBuilder<'a> {
    context: &'a Context,
    value_to_def: &'a HashMap<ValueId, OpId>,
    /// Each region's operations in the order they are solved in: a value is
    /// lowered by its defining operation before anything reading it, whatever
    /// order the region was built in.
    order: &'a HashMap<RegionId, Vec<OpId>>,
    egraph: &'a mut SemEGraph,
    pointer_width: Option<u32>,
    /// The e-class built for each already-lowered IR value (operand sharing / CSE).
    pub(crate) value_to_class: HashMap<ValueId, Id>,
    /// Serial of the next opaque leaf; each un-lowerable node gets its own.
    opaque_serial: u32,
}

impl<'a> SemDagBuilder<'a> {
    pub(crate) fn new(
        context: &'a Context,
        value_to_def: &'a HashMap<ValueId, OpId>,
        order: &'a HashMap<RegionId, Vec<OpId>>,
        egraph: &'a mut SemEGraph,
        pointer_width: Option<u32>,
    ) -> Self {
        Self {
            context,
            value_to_def,
            order,
            egraph,
            pointer_width,
            value_to_class: HashMap::new(),
            opaque_serial: 0,
        }
    }

    /// Lower `region` into the graph, and every region its operations carry.
    pub(crate) fn build_region(
        &mut self,
        region: RegionId,
        float_widths: &HashSet<u32>,
        seeds: &mut Seeds,
    ) {
        let ops = self.order[&region].clone();
        for op_id in ops {
            let op = self.context.get_op(op_id);
            if !op.regions().is_empty() {
                self.build_region_op(&op, float_widths, seeds);
            } else {
                self.build_plain_op(op_id, &op, float_widths, seeds);
            }
        }
    }

    /// Lower one region-free operation, recording the class it is rooted at. A
    /// standalone constant has no semantic root of its own, so it is rooted at the
    /// class its value builds; a float one only where the target declares a
    /// materializer for its width.
    fn build_plain_op(
        &mut self,
        op_id: OpId,
        op: &OpHandle,
        float_widths: &HashSet<u32>,
        seeds: &mut Seeds,
    ) {
        if let Some(root) = self.build_for_op(op).or_else(|| {
            op.is::<crate::builtin::ConstantOp>()
                .then(|| self.build_from_value(op.results()[0]))
        }) {
            seeds.roots_by_op.insert(op_id, root);
        } else if op.is::<crate::builtin::ConstantFOp>()
            && let Some(&result) = op.results().first()
            && self
                .float_width(result)
                .is_some_and(|width| float_widths.contains(&width))
        {
            let class = self.build_from_value(result);
            seeds.constant_candidates.push((op_id, class));
        }
    }

    fn float_width(&self, value: ValueId) -> Option<u32> {
        let ty = self.context.get_value(value).ty();
        let data = self.context.get_type_data(ty);
        (data.as_ref() as &dyn std::any::Any)
            .downcast_ref::<FloatType>()
            .map(FloatType::bit_width)
    }

    /// A structured operation is read through its own binding: its regions join
    /// this graph, and what it publishes is the γ over what its arms produce or
    /// the θ over what its body carries. What its regions did to memory is on
    /// the state ports it carries, so nothing here has to guess at it.
    fn build_region_op(&mut self, op: &OpHandle, float_widths: &HashSet<u32>, seeds: &mut Seeds) {
        if let Some(gamma) = op.clone().as_interface::<dyn Gamma>() {
            let binding = gamma.forwarded();
            let inputs = op.value_operands()[binding.operands.clone()].to_vec();
            for arm in gamma.arms() {
                let region = self.context.get_region(arm);
                let ports = region.value_arguments();
                for (port, &input) in ports[binding.ports.clone()].iter().zip(&inputs) {
                    let class = self.build_from_value(input);
                    self.value_to_class.insert(port.id(), class);
                }
                // A gate forwards its memory states into every arm unchanged,
                // so an access in an arm reads the state the gate was handed.
                for (port, &state) in region.dep_arguments().iter().zip(&op.dep_operands()) {
                    let class = self.build_from_value(state);
                    self.value_to_class.insert(port.id(), class);
                }
                self.build_region(arm, float_widths, seeds);
            }
            self.seed_gamma(op, gamma.as_ref());
            return;
        }
        if let Some(theta) = op.clone().as_interface::<dyn Theta>() {
            self.seed_theta(op, theta.as_ref(), float_widths, seeds);
            return;
        }
        for region in op.regions().to_vec() {
            self.build_region(region, float_widths, seeds);
        }
    }

    /// γ: what a gate publishes is the choice between what its arms produce, one
    /// child per arm in the order the binding reports them — the order a
    /// destruction maps back onto the regions.
    fn seed_gamma(&mut self, op: &OpHandle, gamma: &dyn Gamma) {
        let decision = self.build_from_value(gamma.predicate());
        let arms = gamma.arms();
        let binding = gamma.forwarded();
        for (index, &result) in op.value_results()[binding.results.clone()]
            .iter()
            .enumerate()
        {
            let produced: Vec<ValueId> = arms
                .iter()
                .map(|&arm| {
                    self.context.get_region(arm).value_results()[binding.exit.clone()][index]
                })
                .collect();
            let class = match produced.as_slice() {
                &[value] => self.build_from_value(value),
                values => {
                    let mut args = vec![decision];
                    for &value in values {
                        args.push(self.build_from_value(value));
                    }
                    let ty = self.context.get_value(result).ty();
                    let gamma = self.egraph.add(SemNode::gamma(result, args).typed(ty));
                    // The gate's own value joins the choice: the cover may read the
                    // gate as the register its regions leave it in, whatever it can
                    // do with the arms' terms.
                    let anchor = self.add_input_value(result, Some(ty));
                    self.egraph.union(gamma, anchor)
                }
            };
            self.value_to_class.insert(result, class);
        }
    }

    /// θ: what a loop carries in a port is the value it was entered with and the
    /// one the next iteration carries. The port anchors first, so the body can
    /// be read on it, and the θ joins that class after: the continue value is a
    /// term over the port itself.
    fn seed_theta(
        &mut self,
        op: &OpHandle,
        theta: &dyn Theta,
        float_widths: &HashSet<u32>,
        seeds: &mut Seeds,
    ) {
        let binding = theta.carried();
        let body = theta.body();
        let region = self.context.get_region(body);
        let ports: Vec<ValueId> = region.value_arguments()[binding.ports.clone()]
            .iter()
            .map(|port| port.id())
            .collect();
        for &port in &ports {
            self.build_from_value(port);
        }
        self.build_region(body, float_widths, seeds);

        let inits = op.value_operands()[binding.operands.clone()].to_vec();
        let results = region.value_results();
        let continued = &results[binding.continue_.clone()];
        for ((&port, &init), &next) in ports.iter().zip(&inits).zip(continued) {
            let init = self.build_from_value(init);
            let next = self.build_from_value(next);
            let head = self.build_from_value(port);
            let theta = self.egraph.add(SemNode::theta(port, vec![init, next]));
            self.egraph.union(theta, head);
        }
        self.egraph.rebuild();
    }

    /// Build what destructing `op` will branch on, recording each class against
    /// the region whose cover must materialize it: a gate's arm tests where the
    /// gate sits, a loop's repeat predicate in its body.
    pub(crate) fn build_region_control(
        &mut self,
        op: &OpHandle,
        region: RegionId,
        control: &mut RegionControl,
    ) {
        if let Some(gamma) = op.clone().as_interface::<dyn Gamma>() {
            control.test_conditions.insert((op.id, gamma.predicate()));
            self.build_arm_tests(op, region, gamma.as_ref(), control);
            return;
        }
        if let Some(theta) = op.clone().as_interface::<dyn Theta>() {
            let predicate = theta.predicate();
            control.test_conditions.insert((op.id, predicate));
            let class = self.build_from_value(predicate);
            control.record(theta.body(), op.id, AuxSlot::Test(0), class);
        }
    }

    /// A gate's arms are entered on `predicate == index`, tested in arm order
    /// with the last arm taking whatever is left. A one-bit predicate selects
    /// arm 1 by holding and arm 0 by not, so it stands for itself either way.
    fn build_arm_tests(
        &mut self,
        op: &OpHandle,
        region: RegionId,
        gamma: &dyn Gamma,
        control: &mut RegionControl,
    ) {
        let predicate = gamma.predicate();
        let ty = self.context.get_value(predicate).ty();
        let Some(width) = type_width(self.context, ty) else {
            return;
        };
        let class = self.build_from_value(predicate);
        let boolean = IntegerType::new(self.context, 1);
        for index in 0..gamma.arms().len().saturating_sub(1) {
            let (slot, test) = match (width, index) {
                (1, 0) => (AuxSlot::Unless(0), class),
                (1, _) => (AuxSlot::Test(index), class),
                _ => {
                    let expected = self.add_int(APInt::new(width, index as u64), Some(ty));
                    let test = self.add_op(SymKind::Eq, vec![class, expected], Some(boolean));
                    (AuxSlot::Test(index), test)
                }
            };
            control.record(region, op.id, slot, test);
        }
    }

    fn add_leaf(
        &mut self,
        kind: SymKind,
        payload: Option<SymPayload<ValueId>>,
        ty: Option<TypeId>,
    ) -> Id {
        self.egraph.add(template_node(kind, payload, ty))
    }

    fn next_opaque_serial(&mut self) -> u32 {
        let serial = self.opaque_serial;
        self.opaque_serial += 1;
        serial
    }

    fn add_int(&mut self, value: APInt, ty: Option<TypeId>) -> Id {
        self.add_leaf(SymKind::Constant, Some(SymPayload::Int(value)), ty)
    }

    fn add_u64_const(&mut self, value: u64) -> Id {
        self.add_int(minimal_unsigned_apint(value), None)
    }

    /// Add the `addr + 0` form used by base+offset addressing patterns and record
    /// its exact equality with the bare address used by direct-base patterns.
    ///
    /// The equality is recorded only for pure addresses: an effectful address
    /// (e.g. a loaded pointer) must keep its effect node as the class's sole
    /// materialization — unioning in an arithmetic view would let the cover pick
    /// that view and leave the effect with no rule to materialize it.
    fn zero_offset_address(&mut self, address: Id) -> Id {
        let zero = self.add_u64_const(0);
        let with_zero = self.add_op(SymKind::Add, vec![address, zero], None);
        if class_is_pure(self.egraph, address) {
            self.egraph.union(with_zero, address)
        } else {
            with_zero
        }
    }

    fn add_input_value(&mut self, value: ValueId, ty: Option<TypeId>) -> Id {
        self.add_leaf(SymKind::Symbol, Some(SymPayload::Value(value)), ty)
    }

    fn add_unknown_symbol(&mut self, symbol: u32, ty: Option<TypeId>) -> Id {
        self.add_leaf(SymKind::Symbol, Some(SymPayload::SymbolId(symbol)), ty)
    }

    /// A leaf that nothing materializes — the placeholder for an un-lowerable node,
    /// so a partial semantic expansion still yields a well-formed graph. Each call
    /// mints a distinct leaf: two unknown computations are never assumed equal.
    pub(crate) fn add_opaque(&mut self) -> Id {
        let serial = self.next_opaque_serial();
        let mut node = template_node(SymKind::Symbol, None, None);
        node.payload = Some(SemPayload::Opaque(serial));
        self.egraph.add(node)
    }

    /// Build an operator node, canonicalizing commutative operands so `a op b` and
    /// `b op a` hash-cons to the same e-node (mirroring the program's CSE).
    fn add_op(&mut self, kind: SymKind, mut children: Vec<Id>, ty: Option<TypeId>) -> Id {
        if kind.is_commutative() {
            children.sort();
        }
        let mut node = template_node(kind, None, ty);
        node.children = children;
        self.egraph.add(node)
    }

    pub(crate) fn build_for_op(&mut self, op: &OpHandle) -> Option<Id> {
        // A standalone `constantf` is left for the target's pre-RA hook, like a
        // bare integer `constant`; only as an operand (see `build_from_value`)
        // does it fold into a consumer via `float_constant_class`.
        if op.is::<crate::builtin::ConstantFOp>() {
            return None;
        }
        // A memory access names its own results: what it reads is the term, and
        // what it publishes is the chain the accesses after it read.
        if let Some(class) = self.build_memory_effect(op) {
            return Some(class);
        }
        let operands = self.build_operands(&op.operands());
        let mut graph = SemGraph::new();
        let root = op.clone().as_dyn_op().semantic_expr(&mut graph)?;
        let class = self.lower_typed(&graph, root, &operands);
        for result in op.results() {
            self.value_to_class.insert(result, class);
        }
        Some(class)
    }

    fn build_operands(&mut self, operands: &[ValueId]) -> Vec<Id> {
        operands
            .iter()
            .map(|&operand| self.build_from_value(operand))
            .collect()
    }

    fn lower_typed(&mut self, graph: &SemGraph, root: NodeId, operands: &[Id]) -> Id {
        let types = self.infer_local_types(graph, operands);
        self.lower_graph_node(graph, root, operands, types.as_deref())
    }

    /// Lower a memory access over the chain the IR says it reads. The state is an
    /// ordinary operand: `state.entry_state`, a block argument and `state.join`
    /// are leaves, and a write's own term is the state the accesses after it
    /// read. Nothing here invents an order — the mid-end's chains are the whole
    /// of memory identity, and the ports the term is built from are the ones
    /// emission threads through the machine instruction covering it.
    fn build_memory_effect(&mut self, op: &OpHandle) -> Option<Id> {
        if let Some(read) = op.clone().as_interface::<dyn MemoryRead>() {
            let result = read.read_value();
            let result_ty = self.context.get_value(result).ty();
            let bytes = self.type_width(result_ty)? / 8;
            let observed = read.state_operand()?;
            let address = self.build_from_value(read.read_location());
            let address = self.zero_offset_address(address);
            let bytes = self.add_u64_const(u64::from(bytes));
            let metadata = self.add_u64_const(0);
            let state = self.build_from_value(observed);
            // A read leaves memory as it found it, so the chain runs on
            // unchanged; two reads of one address on one chain are one term.
            let class = self.add_op(
                SymKind::LoadMemory,
                vec![address, bytes, metadata, state],
                Some(result_ty),
            );
            self.value_to_class.insert(result, class);
            if let Some(published) = read.state_result() {
                self.value_to_class.insert(published, state);
            }
            return Some(class);
        }

        if let Some(write) = op.clone().as_interface::<dyn MemoryWrite>() {
            let written = write.written_value();
            let value_ty = self.context.get_value(written).ty();
            let bytes = self.type_width(value_ty)? / 8;
            let observed = write.state_operand()?;
            let published = write.state_result()?;
            let address = self.build_from_value(write.write_location());
            let address = self.zero_offset_address(address);
            let bytes = self.add_u64_const(u64::from(bytes));
            let value = self.build_from_value(written);
            let address_space = self.add_u64_const(0);
            let state = self.build_from_value(observed);
            let class = self.add_op(
                SymKind::StoreMemory,
                vec![address, bytes, value, address_space, state],
                None,
            );
            self.value_to_class.insert(published, class);
            return Some(class);
        }

        None
    }

    fn type_width(&self, ty: TypeId) -> Option<u32> {
        type_width(self.context, ty).or_else(|| {
            let data = self.context.get_type_data(ty);
            (data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<tir::ptr::PtrType>()
                .and(self.pointer_width)
        })
    }

    /// The canonical comparison the (possibly cross-block) definer of `value`
    /// computes, lowered into this graph: `(class, kind, lhs class, rhs class)`.
    /// `None` when the definer is unknown, effectful, or not a comparison. Used
    /// by dominating-edge assumptions to relate a guard condition to the values
    /// it compares.
    pub(crate) fn build_defining_compare(
        &mut self,
        value: ValueId,
    ) -> Option<(Id, SymKind, Id, Id)> {
        let def_id = self.context.get_value(value).defining_op()?;
        if !self.context.has_operation(def_id) {
            return None;
        }
        let def = self.context.get_op(def_id);
        if def.results().len() != 1
            || def.clone().as_interface::<dyn MemoryRead>().is_some()
            || def.clone().as_interface::<dyn MemoryWrite>().is_some()
        {
            return None;
        }
        let mut graph = SemGraph::new();
        let root = def.clone().as_dyn_op().semantic_expr(&mut graph)?;
        let operands = self.build_operands(&def.operands());
        let class = self.lower_typed(&graph, root, &operands);
        let comparison = self
            .egraph
            .nodes(class)
            .find(|n| n.sym().is_some_and(tir::sem::egraph::is_comparison))?
            .clone();
        Some((
            class,
            comparison
                .sym()
                .expect("a comparison node is a semantic operator"),
            comparison.children[0],
            comparison.children[1],
        ))
    }

    pub(crate) fn build_from_value(&mut self, value: ValueId) -> Id {
        if let Some(existing) = self.value_to_class.get(&value) {
            return *existing;
        }

        let value_ty = Some(self.context.get_value(value).ty());
        let class = if let Some(def_op_id) = self.value_to_def.get(&value).copied() {
            let def = self.context.get_op(def_op_id);
            if def.is::<crate::builtin::ConstantOp>() {
                self.constant_class(&def, value, value_ty)
            } else if def.is::<crate::builtin::ConstantFOp>() {
                self.float_constant_class(&def)
                    .unwrap_or_else(|| self.add_input_value(value, value_ty))
            } else {
                let mut graph = SemGraph::new();
                if let Some(root) = def.clone().as_dyn_op().semantic_expr(&mut graph) {
                    let operands = self.build_operands(&def.operands());
                    self.lower_typed(&graph, root, &operands)
                } else {
                    self.add_input_value(value, value_ty)
                }
            }
        } else {
            self.add_input_value(value, value_ty)
        };

        self.value_to_class.insert(value, class);
        class
    }

    /// Lower a `constant` op to an integer-literal leaf, or to an input value when its
    /// payload is not an integer.
    fn constant_class(&mut self, def: &OpHandle, value: ValueId, value_ty: Option<TypeId>) -> Id {
        match def.attr("value") {
            Some(AttributeValue::Int(v)) => {
                let width = value_ty
                    .and_then(|ty| {
                        let ty = self.context.get_type_data(ty);
                        (ty.as_ref() as &dyn std::any::Any)
                            .downcast_ref::<tir::builtin::IntegerType>()
                            .map(tir::builtin::IntegerType::width)
                    })
                    .unwrap_or(64);
                self.add_int(APInt::new_signed(width, v), value_ty)
            }
            _ => self.add_input_value(value, value_ty),
        }
    }

    fn float_constant_class(&mut self, def: &OpHandle) -> Option<Id> {
        let &result = def.results().first()?;
        let result_ty = self.context.get_value(result).ty();
        let width = {
            let data = self.context.get_type_data(result_ty);
            (data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<FloatType>()?
                .bit_width()
        };
        let value = match def.attr("value")? {
            AttributeValue::F64(value) => value,
            _ => return None,
        };
        let bits = match width {
            32 => (value as f32).to_bits() as i32 as i64,
            64 => value.to_bits() as i64,
            _ => return None,
        };
        let int_ty = IntegerType::new(self.context, width);
        let bits = self.add_int(APInt::new_signed(64, bits), Some(int_ty));
        Some(self.add_op(SymKind::Bitcast, vec![bits], Some(result_ty)))
    }

    fn infer_local_types(&self, graph: &SemGraph, operands: &[Id]) -> Option<Vec<SemType>> {
        infer_types(graph, |node| {
            graph
                .get_actual_type(node)
                .and_then(|ty| semantic_type(self.context, ty))
                .or_else(|| match graph.get_leaf_data(node) {
                    Some(SymPayload::SymbolId(id)) => operands
                        .get(*id as usize)
                        .and_then(|&class| self.class_ty(class))
                        .and_then(|ty| semantic_type(self.context, ty)),
                    _ => None,
                })
        })
        .ok()
    }

    /// The IR type recorded on an operand class (taken from any member carrying one).
    fn class_ty(&self, class: Id) -> Option<TypeId> {
        self.egraph.nodes(class).find_map(|n| n.ty)
    }

    fn lower_graph_node(
        &mut self,
        graph: &SemGraph,
        node: NodeId,
        operands: &[Id],
        types: Option<&[SemType]>,
    ) -> Id {
        let node_ty = graph
            .get_actual_type(node)
            .or_else(|| types.and_then(|types| ir_type(self.context, &types[node.index()])));
        match graph.get_node(node) {
            SymKind::Symbol => match graph.get_leaf_data(node) {
                Some(SymPayload::SymbolId(id)) => operands
                    .get(*id as usize)
                    .copied()
                    .unwrap_or_else(|| self.add_unknown_symbol(*id, node_ty)),
                _ => self.add_opaque(),
            },
            SymKind::Constant => match graph.get_leaf_data(node) {
                Some(SymPayload::Int(v)) => self.add_int(v.clone(), node_ty),
                _ => self.add_opaque(),
            },
            kind => {
                let children: Vec<Id> = graph
                    .children(node)
                    .map(|child| self.lower_graph_node(graph, child, operands, types))
                    .collect();
                if kind.accepts_arity(children.len()) {
                    self.add_op(*kind, children, node_ty)
                } else {
                    self.add_opaque()
                }
            }
        }
    }
}
