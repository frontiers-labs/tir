//! Lowering of a block's IR operations into the semantic e-graph.

use std::collections::HashMap;

use tir::{
    Context, MemoryRead, MemoryWrite, OpId, OpInstance, TypeId, ValueId,
    attributes::AttributeValue,
    graph::{Dag, NodeId},
    sem::{SemGraph, SymKind, SymPayload, infer_widths},
};
use tir_adt::APInt;
use tir_symbolic::egraph::Id;

use super::node::{
    SemEGraph, SemNode, SemPayload, minimal_unsigned_apint, template_node, type_for_kind_width,
    type_width,
};

/// Builds a block's semantic expressions straight into the e-graph: every lowered
/// node is hash-consed by [`SemEGraph::add`], so the e-graph *is* the interned DAG
/// (no separate arena). Returns e-class [`Id`]s and records, in `value_to_class`,
/// the class built for each IR value so operands share and cross-block uses expand.
pub(crate) struct SemDagBuilder<'a> {
    context: &'a Context,
    value_to_def: &'a HashMap<ValueId, OpId>,
    egraph: &'a mut SemEGraph,
    /// The e-class built for each already-lowered IR value (operand sharing / CSE).
    pub(crate) value_to_class: HashMap<ValueId, Id>,
    /// Serial of the next opaque leaf; each un-lowerable node gets its own.
    opaque_serial: u32,
}

impl<'a> SemDagBuilder<'a> {
    pub(crate) fn new(
        context: &'a Context,
        value_to_def: &'a HashMap<ValueId, OpId>,
        egraph: &'a mut SemEGraph,
    ) -> Self {
        Self {
            context,
            value_to_def,
            egraph,
            value_to_class: HashMap::new(),
            opaque_serial: 0,
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

    /// The synthetic `addr + 0` wrapper that makes a bare pointer match the
    /// targets' base+offset addressing patterns (canonicalized to `Add(base, imm)`).
    /// The `Add` is private to one memory op (see [`Self::add_op_unique`]), like
    /// the memory effect it addresses: two memory ops are never interchangeable,
    /// so neither is their addressing context.
    fn zero_offset_address(&mut self, address: Id) -> Id {
        let zero = self.add_u64_const(0);
        self.add_op_unique(SymKind::Add, vec![address, zero], None)
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
        self.egraph.add(SemNode {
            kind: SymKind::Symbol,
            payload: Some(SemPayload::Opaque(serial)),
            ty: None,
            children: Vec::new(),
        })
    }

    /// Build an operator node, canonicalizing commutative operands so `a op b` and
    /// `b op a` hash-cons to the same e-node (mirroring the program's CSE).
    fn add_op_node(
        &mut self,
        kind: SymKind,
        mut children: Vec<Id>,
        ty: Option<TypeId>,
        payload: Option<SemPayload>,
    ) -> Id {
        if kind.is_commutative() {
            children.sort();
        }
        self.egraph.add(SemNode {
            kind,
            payload,
            ty,
            children,
        })
    }

    fn add_op(&mut self, kind: SymKind, children: Vec<Id>, ty: Option<TypeId>) -> Id {
        self.add_op_node(kind, children, ty, None)
    }

    /// Like [`Self::add_op`], but never hash-conses with another node: the opaque
    /// serial in the payload keeps the label distinct, and an untyped pattern node
    /// of the same kind still matches it (a pattern payload of `None` is a
    /// wildcard). Used for memory effects and their addressing arithmetic, which
    /// are not pure values: two loads of the same address are not interchangeable
    /// across an intervening store, so their e-classes must never merge.
    fn add_op_unique(&mut self, kind: SymKind, children: Vec<Id>, ty: Option<TypeId>) -> Id {
        let payload = Some(SemPayload::Opaque(self.next_opaque_serial()));
        self.add_op_node(kind, children, ty, payload)
    }

    pub(crate) fn build_for_op(&mut self, op: &std::sync::Arc<OpInstance>) -> Option<Id> {
        if let Some(class) = self.build_memory_effect(op) {
            return Some(class);
        }

        let operands = self.build_operands(&op.operands);
        let mut graph = SemGraph::new();
        let root = op.clone().as_dyn_op().semantic_expr(&mut graph)?;
        Some(self.lower_with_widths(&graph, root, &operands))
    }

    fn build_operands(&mut self, operands: &[ValueId]) -> Vec<Id> {
        operands
            .iter()
            .map(|&operand| self.build_from_value(operand))
            .collect()
    }

    /// Infer node widths from `operands` and lower `graph` rooted at `root`.
    fn lower_with_widths(&mut self, graph: &SemGraph, root: NodeId, operands: &[Id]) -> Id {
        let widths = self.infer_local_widths(graph, operands);
        self.lower_graph_node(graph, root, operands, &widths)
    }

    fn build_memory_effect(&mut self, op: &std::sync::Arc<OpInstance>) -> Option<Id> {
        let read_parts = op
            .clone()
            .as_interface::<dyn MemoryRead>()
            .map(|read| (read.read_location(), read.read_value()));

        if let Some((location, result)) = read_parts {
            let result_ty = self.context.get_value(result).ty();
            let bytes = type_width(self.context, result_ty)? / 8;
            let address = self.build_from_value(location);
            let address = self.zero_offset_address(address);
            let bytes = self.add_u64_const(u64::from(bytes));
            let metadata = self.add_u64_const(0);
            return Some(self.add_op_unique(
                SymKind::LoadMemory,
                vec![address, bytes, metadata],
                Some(result_ty),
            ));
        }

        let write_parts = op
            .clone()
            .as_interface::<dyn MemoryWrite>()
            .map(|write| (write.write_location(), write.written_value()));

        if let Some((location, value)) = write_parts {
            let value_ty = self.context.get_value(value).ty();
            let bytes = type_width(self.context, value_ty)? / 8;
            let address = self.build_from_value(location);
            let address = self.zero_offset_address(address);
            let bytes = self.add_u64_const(u64::from(bytes));
            let value = self.build_from_value(value);
            let address_space = self.add_u64_const(0);
            return Some(self.add_op_unique(
                SymKind::StoreMemory,
                vec![address, bytes, value, address_space],
                None,
            ));
        }

        None
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
        if def.results.len() != 1
            || def.clone().as_interface::<dyn MemoryRead>().is_some()
            || def.clone().as_interface::<dyn MemoryWrite>().is_some()
        {
            return None;
        }
        let mut graph = SemGraph::new();
        let root = def.clone().as_dyn_op().semantic_expr(&mut graph)?;
        let operands = self.build_operands(&def.operands);
        let class = self.lower_with_widths(&graph, root, &operands);
        let comparison = self
            .egraph
            .nodes(class)
            .iter()
            .find(|n| super::node::is_comparison(n.kind))?
            .clone();
        Some((
            class,
            comparison.kind,
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
            if def.name == "constant" {
                self.constant_class(&def, value, value_ty)
            } else {
                let mut graph = SemGraph::new();
                if let Some(root) = def.clone().as_dyn_op().semantic_expr(&mut graph) {
                    let operands = self.build_operands(&def.operands);
                    self.lower_with_widths(&graph, root, &operands)
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
    fn constant_class(
        &mut self,
        def: &std::sync::Arc<OpInstance>,
        value: ValueId,
        value_ty: Option<TypeId>,
    ) -> Id {
        match def.attributes.iter().find(|a| a.name == "value") {
            Some(attr) => match &attr.value {
                AttributeValue::Int(v) => self.add_int(APInt::new_signed(64, *v), value_ty),
                _ => self.add_input_value(value, value_ty),
            },
            None => self.add_input_value(value, value_ty),
        }
    }

    /// Infer the width of every node of `graph` from the IR types of the operands
    /// it references, then resolve those widths against the live context. This is
    /// the same width rule TMDL uses for patterns, so the program graph and the
    /// rule patterns end up typed consistently.
    fn infer_local_widths(&self, graph: &SemGraph, operands: &[Id]) -> Vec<Option<u32>> {
        infer_widths(graph, |node| match graph.get_leaf_data(node) {
            Some(SymPayload::SymbolId(id)) => operands
                .get(*id as usize)
                .and_then(|&class| self.class_ty(class))
                .and_then(|ty| type_width(self.context, ty)),
            _ => None,
        })
    }

    /// The IR type recorded on an operand class (taken from any member carrying one).
    fn class_ty(&self, class: Id) -> Option<TypeId> {
        self.egraph.nodes(class).iter().find_map(|n| n.ty)
    }

    /// Lower one node of a semantic-expression graph, typing each node from its
    /// inferred width. Operand leaves keep the IR type they were built with;
    /// internal nodes (and the root) take their inferred width resolved to a
    /// type — a float type for the float kinds, so float-typed rule patterns
    /// match them exactly.
    fn lower_graph_node(
        &mut self,
        graph: &SemGraph,
        node: NodeId,
        operands: &[Id],
        widths: &[Option<u32>],
    ) -> Id {
        let node_ty = widths[node.index()]
            .and_then(|width| type_for_kind_width(self.context, *graph.get_node(node), width));
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
                    .map(|child| self.lower_graph_node(graph, child, operands, widths))
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
