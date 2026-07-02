use std::collections::{HashMap, HashSet};

use crate::NodeId;

/// Capability a graph-node label needs to take part in pattern matching /
/// e-matching: leaf classification, equality against a template label, and the
/// operand-kind predicates. Kept separate from [`crate::Dag`], which is a
/// pure storage view, so analyses that never match patterns (e.g. dominator trees)
/// owe it nothing.
pub trait Matchable<C> {
    fn is_leaf(&self, ctx: &C) -> bool;

    fn num_children(&self, ctx: &C) -> usize;

    fn matches_pattern(&self, template: &Self, _ctx: &C) -> bool
    where
        Self: PartialEq + Sized,
    {
        self == template
    }

    fn is_commutative(&self) -> bool {
        false
    }

    /// Whether this node is a compile-time constant, distinguishing immediate
    /// operands (which must bind to a constant) from register operands (which must
    /// not) during matching.
    fn is_constant(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternExpr<N> {
    Any,
    Boundary,
    Leaf,
    Node(N),
}

/// Restricts what a boundary (operand) pattern node may bind to, distinguishing
/// register operands from immediates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandConstraint {
    /// Must bind to a non-constant value (a register / SSA value).
    Register,
    /// Must bind to a compile-time constant (an immediate).
    Immediate,
}

pub struct Pattern<N, A> {
    nodes: Vec<PatternExpr<N>>,
    edges: HashMap<NodeId, Vec<NodeId>>,
    parents: HashMap<NodeId, Vec<NodeId>>,
    duplicable: HashSet<NodeId>,
    operand_constraints: HashMap<NodeId, OperandConstraint>,
    root: Option<NodeId>,
    applicator: A,
}

impl<N, A> Pattern<N, A> {
    pub fn new(a: A) -> Self {
        Self {
            nodes: vec![],
            edges: HashMap::new(),
            parents: HashMap::new(),
            duplicable: HashSet::new(),
            operand_constraints: HashMap::new(),
            root: None,
            applicator: a,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn applicator(&self) -> &A {
        &self.applicator
    }

    pub fn add_node(&mut self, node: PatternExpr<N>) -> NodeId {
        let id = NodeId::from_index(self.nodes.len());
        self.nodes.push(node);
        id
    }

    pub fn add_edge(&mut self, from: NodeId, to: NodeId) {
        self.edges.entry(from).or_default().push(to);
        self.parents.entry(to).or_default().push(from);
    }

    pub fn set_duplicable(&mut self, node: NodeId, duplicable: bool) {
        if duplicable {
            self.duplicable.insert(node);
        } else {
            self.duplicable.remove(&node);
        }
    }

    pub fn is_duplicable(&self, node: NodeId) -> bool {
        self.duplicable.contains(&node)
    }

    pub fn set_operand_constraint(&mut self, node: NodeId, constraint: OperandConstraint) {
        self.operand_constraints.insert(node, constraint);
    }

    pub fn operand_constraint(&self, node: NodeId) -> Option<OperandConstraint> {
        self.operand_constraints.get(&node).copied()
    }

    pub fn set_root(&mut self, root: NodeId) {
        self.root = Some(root);
    }

    pub fn root(&self) -> Option<NodeId> {
        self.root.or_else(|| self.infer_root())
    }

    pub fn get_node(&self, id: NodeId) -> &PatternExpr<N> {
        &self.nodes[id.index()]
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        self.edges.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    fn parents(&self, id: NodeId) -> &[NodeId] {
        self.parents.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    fn infer_root(&self) -> Option<NodeId> {
        let mut roots = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, _)| NodeId::from_index(i))
            .filter(|&id| self.parents(id).is_empty());
        let root = roots.next()?;
        if roots.next().is_some() {
            return None;
        }
        Some(root)
    }
}
