//! Lowering of a block's IR operations into the semantic e-graph.

use std::collections::HashMap;

use tir::{
    Context, MemoryRead, MemoryWrite, OpId, OpInstance, TypeId, ValueId,
    attributes::AttributeValue,
    builtin::IntegerType,
    egraph::EClassId,
    graph::{Dag, Matchable, NodeId},
    sem_expr::{ExprKind, ExprPayload, ExprPostGraph, infer_widths},
    utils::APInt,
};

use super::node::{SemEGraph, SemNode, SemPayload, minimal_unsigned_apint, type_width};

/// Builds a block's semantic expressions straight into the e-graph: every lowered
/// node is hash-consed by [`SemEGraph::add`], so the e-graph *is* the interned DAG
/// (no separate arena). Returns [`EClassId`]s and records, in `class_value`, which
/// class computes which op result so an intermediate can later be materialized as a
/// register value.
pub(crate) struct SemDagBuilder<'a> {
    context: &'a Context,
    value_to_def: &'a HashMap<ValueId, OpId>,
    egraph: &'a mut SemEGraph,
    /// The e-class built for each already-lowered IR value (operand sharing / CSE).
    value_to_class: HashMap<ValueId, EClassId>,
    /// First class found to compute each op result (first writer wins, matching CSE).
    pub(crate) class_value: HashMap<EClassId, ValueId>,
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
            class_value: HashMap::new(),
            opaque_serial: 0,
        }
    }

    fn add_leaf(
        &mut self,
        kind: ExprKind,
        payload: Option<ExprPayload>,
        ty: Option<TypeId>,
    ) -> EClassId {
        self.egraph.add(
            SemNode {
                kind,
                payload: payload.map(SemPayload::Expr),
                ty,
            },
            &[],
            None,
        )
    }

    fn add_int(&mut self, value: APInt, ty: Option<TypeId>) -> EClassId {
        self.add_leaf(ExprKind::Constant, Some(ExprPayload::Int(value)), ty)
    }

    fn add_u64_const(&mut self, value: u64) -> EClassId {
        self.add_int(minimal_unsigned_apint(value), None)
    }

    /// The synthetic `addr + 0` wrapper that makes a bare pointer match the
    /// targets' base+offset addressing patterns (canonicalized to `Add(base, imm)`).
    /// The `Add` is private to one memory op (see [`Self::add_op_unique`]), like
    /// the memory effect it addresses: two memory ops are never interchangeable,
    /// so neither is their addressing context.
    fn zero_offset_address(&mut self, address: EClassId) -> EClassId {
        let zero = self.add_u64_const(0);
        self.add_op_unique(ExprKind::Add, vec![address, zero], None)
    }

    fn add_input_value(&mut self, value: ValueId, ty: Option<TypeId>) -> EClassId {
        self.add_leaf(ExprKind::Symbol, Some(ExprPayload::Value(value)), ty)
    }

    fn add_unknown_symbol(&mut self, symbol: u32, ty: Option<TypeId>) -> EClassId {
        self.add_leaf(ExprKind::Symbol, Some(ExprPayload::SymbolId(symbol)), ty)
    }

    /// A leaf that nothing materializes — the placeholder for an un-lowerable node,
    /// so a partial semantic expansion still yields a well-formed graph. Each call
    /// mints a distinct leaf: two unknown computations are never assumed equal.
    pub(crate) fn add_opaque(&mut self) -> EClassId {
        let serial = self.opaque_serial;
        self.opaque_serial += 1;
        self.egraph.add(
            SemNode {
                kind: ExprKind::Symbol,
                payload: Some(SemPayload::Opaque(serial)),
                ty: None,
            },
            &[],
            None,
        )
    }

    fn add_op(
        &mut self,
        kind: ExprKind,
        mut children: Vec<EClassId>,
        ty: Option<TypeId>,
    ) -> EClassId {
        // Canonicalize commutative operands so `a op b` and `b op a` hash-cons to the
        // same e-node, mirroring the program's CSE.
        if kind.is_commutative() {
            children.sort();
        }
        self.egraph.add(
            SemNode {
                kind,
                payload: None,
                ty,
            },
            &children,
            None,
        )
    }

    /// Like [`Self::add_op`], but never hash-conses with another node: the opaque
    /// serial in the payload keeps the label distinct, and an untyped pattern node
    /// of the same kind still matches it (a pattern payload of `None` is a
    /// wildcard). Used for memory effects and their addressing arithmetic, which
    /// are not pure values: two loads of the same address are not interchangeable
    /// across an intervening store, so their e-classes must never merge.
    fn add_op_unique(
        &mut self,
        kind: ExprKind,
        mut children: Vec<EClassId>,
        ty: Option<TypeId>,
    ) -> EClassId {
        if kind.is_commutative() {
            children.sort();
        }
        let serial = self.opaque_serial;
        self.opaque_serial += 1;
        self.egraph.add(
            SemNode {
                kind,
                payload: Some(SemPayload::Opaque(serial)),
                ty,
            },
            &children,
            None,
        )
    }

    /// Record that `class` computes IR `value` (idempotent; first writer wins, which
    /// is correct since identical computations are the same value under CSE).
    fn set_value(&mut self, class: EClassId, value: ValueId) {
        self.class_value
            .entry(self.egraph.find(class))
            .or_insert(value);
    }

    pub(crate) fn build_for_op(&mut self, op: &std::sync::Arc<OpInstance>) -> Option<EClassId> {
        if let Some(class) = self.build_branch_effect(op) {
            return Some(class);
        }

        if let Some(class) = self.build_memory_effect(op) {
            return Some(class);
        }

        let mut operands = Vec::with_capacity(op.operands.len());
        for operand in &op.operands {
            operands.push(self.build_from_value(*operand));
        }
        let mut graph = ExprPostGraph::new();
        let root = op.clone().as_dyn_op().semantic_expr(&mut graph)?;
        let widths = self.infer_local_widths(&graph, &operands);
        let class = self.lower_graph_node(&graph, root, &operands, &widths);
        if let Some(result) = op.results.first() {
            self.set_value(class, *result);
        }
        Some(class)
    }

    /// Lower an `asm.condbr` into a `CondBranch` effect node over its condition.
    /// Like a memory effect it is unique (never merged) and never a pure value, so
    /// it roots a match and is never internalized; the branch target is an op
    /// attribute resolved at emit time, so it does not appear in the expression.
    fn build_branch_effect(&mut self, op: &std::sync::Arc<OpInstance>) -> Option<EClassId> {
        if op.name != "condbr" {
            return None;
        }
        let condition = self.build_from_value(*op.operands.first()?);
        Some(self.add_op_unique(ExprKind::CondBranch, vec![condition], None))
    }

    fn build_memory_effect(&mut self, op: &std::sync::Arc<OpInstance>) -> Option<EClassId> {
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
            let class = self.add_op_unique(
                ExprKind::LoadMemory,
                vec![address, bytes, metadata],
                Some(result_ty),
            );
            self.set_value(class, result);
            return Some(class);
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
                ExprKind::StoreMemory,
                vec![address, bytes, value, address_space],
                None,
            ));
        }

        None
    }

    fn build_from_value(&mut self, value: ValueId) -> EClassId {
        if let Some(existing) = self.value_to_class.get(&value) {
            return *existing;
        }

        let value_ty = Some(self.context.get_value(value).ty());
        let class = if let Some(def_op_id) = self.value_to_def.get(&value) {
            let def = self.context.get_op(*def_op_id);
            if def.name == "constant" {
                match def.attributes.iter().find(|a| a.name == "value") {
                    Some(attr) => match &attr.value {
                        AttributeValue::Int(v) => self.add_int(APInt::new_signed(64, *v), value_ty),
                        _ => self.add_input_value(value, value_ty),
                    },
                    None => self.add_input_value(value, value_ty),
                }
            } else {
                let mut graph = ExprPostGraph::new();
                if let Some(root) = def.clone().as_dyn_op().semantic_expr(&mut graph) {
                    let mut operands = Vec::with_capacity(def.operands.len());
                    for operand in &def.operands {
                        operands.push(self.build_from_value(*operand));
                    }
                    let widths = self.infer_local_widths(&graph, &operands);
                    let class = self.lower_graph_node(&graph, root, &operands, &widths);
                    self.set_value(class, value);
                    class
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

    /// Infer the width of every node of `graph` from the IR types of the operands
    /// it references, then resolve those widths against the live context. This is
    /// the same width rule TMDL uses for patterns, so the program graph and the
    /// rule patterns end up typed consistently.
    fn infer_local_widths(&self, graph: &ExprPostGraph, operands: &[EClassId]) -> Vec<Option<u32>> {
        infer_widths(graph, |node| match graph.get_leaf_data(node) {
            Some(ExprPayload::SymbolId(id)) => operands
                .get(*id as usize)
                .and_then(|&class| self.class_ty(class))
                .and_then(|ty| type_width(self.context, ty)),
            _ => None,
        })
    }

    /// The IR type recorded on an operand class (taken from any member carrying one).
    fn class_ty(&self, class: EClassId) -> Option<TypeId> {
        self.egraph
            .nodes(class)
            .iter()
            .find_map(|&id| self.egraph.get_node(id).ty)
    }

    /// Lower one node of a semantic-expression graph, typing each node from its
    /// inferred width. Operand leaves keep the IR type they were built with;
    /// internal nodes (and the root) take their inferred width resolved to a type.
    fn lower_graph_node(
        &mut self,
        graph: &ExprPostGraph,
        node: NodeId,
        operands: &[EClassId],
        widths: &[Option<u32>],
    ) -> EClassId {
        let node_ty = widths[node.index()].map(|width| IntegerType::new(self.context, width));
        match graph.get_node(node) {
            ExprKind::Symbol => match graph.get_leaf_data(node) {
                Some(ExprPayload::SymbolId(id)) => operands
                    .get(*id as usize)
                    .copied()
                    .unwrap_or_else(|| self.add_unknown_symbol(*id, node_ty)),
                _ => self.add_opaque(),
            },
            ExprKind::Constant => match graph.get_leaf_data(node) {
                Some(ExprPayload::Int(v)) => self.add_int(v.clone(), node_ty),
                _ => self.add_opaque(),
            },
            kind => {
                let children: Vec<EClassId> = graph
                    .children(node)
                    .map(|child| self.lower_graph_node(graph, child, operands, widths))
                    .collect();
                if kind.num_children(self.context) == children.len() {
                    self.add_op(*kind, children, node_ty)
                } else {
                    self.add_opaque()
                }
            }
        }
    }
}
