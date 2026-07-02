use std::collections::HashMap;

mod pattern;
mod postorder;

pub use pattern::{Matchable, OperandConstraint, Pattern, PatternExpr};
pub use postorder::PostOrderDag;

pub(crate) static EMPTY_CHILDREN: [NodeId; 0] = [];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    pub fn from_index(i: usize) -> Self {
        NodeId(i as u32)
    }
}

/// A pure read-only view over a cache-friendly graph store, optimized for a
/// particular traversal order by the implementor. Deliberately knows nothing about
/// pattern matching — node labels gain that capability separately via
/// [`Matchable`], required only by the e-graph. Per-node side data the storage
/// carries verbatim (e.g. source provenance) is exposed through the opaque
/// [`Dag::Annotation`] type, which the storage never interprets.
pub trait Dag {
    type Node;
    type Leaf;
    type Annotation;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get_node(&self, id: NodeId) -> &Self::Node;
    fn get_kind(&self, id: NodeId) -> &Self::Node {
        self.get_node(id)
    }
    fn get_leaf_data(&self, id: NodeId) -> Option<&Self::Leaf>;
    fn get_annotation(&self, id: NodeId) -> Option<&Self::Annotation>;

    fn root(&self) -> Option<NodeId>;
    fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId>;

    fn postorder(&self, start: NodeId) -> impl Iterator<Item = NodeId>;
    fn preorder(&self, start: NodeId) -> impl Iterator<Item = NodeId>;
}

pub trait MutDag: Dag {
    fn add_node(&mut self, n: Self::Node) -> NodeId;
    fn add_edge(&mut self, from: NodeId, to: NodeId);
    fn set_leaf_data(&mut self, n: NodeId, d: Self::Leaf);
    fn set_annotation(&mut self, n: NodeId, a: Self::Annotation);
}

pub struct GenericDag<N, L, A = ()> {
    nodes: Vec<N>,
    edges: HashMap<NodeId, Vec<NodeId>>,
    data: HashMap<NodeId, L>,
    annotations: HashMap<NodeId, A>,
}

impl<N, L, A> Default for GenericDag<N, L, A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N, L, A> GenericDag<N, L, A> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: HashMap::new(),
            data: HashMap::new(),
            annotations: HashMap::new(),
        }
    }

    fn contains_descendant(&self, root: NodeId, target: NodeId) -> bool {
        if root == target {
            return true;
        }

        self.edges.get(&root).is_some_and(|children| {
            children
                .iter()
                .any(|&child| self.contains_descendant(child, target))
        })
    }

    fn nth_preorder(&self, node: NodeId, remaining: &mut usize) -> Option<NodeId> {
        if *remaining == 0 {
            return Some(node);
        }
        *remaining -= 1;

        self.edges.get(&node).and_then(|children| {
            for &child in children {
                if let Some(found) = self.nth_preorder(child, remaining) {
                    return Some(found);
                }
            }
            None
        })
    }
}

pub struct GenericDagPostorderIter<'a, N, L, A> {
    dag: &'a GenericDag<N, L, A>,
    start: NodeId,
    next_index: usize,
}

impl<N, L, A> Iterator for GenericDagPostorderIter<'_, N, L, A> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_index <= self.start.index() {
            let candidate = NodeId::from_index(self.next_index);
            self.next_index += 1;

            if self.dag.contains_descendant(self.start, candidate) {
                return Some(candidate);
            }
        }

        None
    }
}

pub struct GenericDagPreorderIter<'a, N, L, A> {
    dag: &'a GenericDag<N, L, A>,
    start: NodeId,
    next_ordinal: usize,
}

impl<N, L, A> Iterator for GenericDagPreorderIter<'_, N, L, A> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        let mut remaining = self.next_ordinal;
        let next = self.dag.nth_preorder(self.start, &mut remaining)?;
        self.next_ordinal += 1;
        Some(next)
    }
}

impl<N, L, A> Dag for GenericDag<N, L, A> {
    type Node = N;
    type Leaf = L;
    type Annotation = A;

    fn len(&self) -> usize {
        self.nodes.len()
    }

    fn get_node(&self, id: NodeId) -> &Self::Node {
        &self.nodes[id.index()]
    }

    fn get_leaf_data(&self, id: NodeId) -> Option<&Self::Leaf> {
        self.data.get(&id)
    }

    fn get_annotation(&self, id: NodeId) -> Option<&Self::Annotation> {
        self.annotations.get(&id)
    }

    fn root(&self) -> Option<NodeId> {
        self.nodes.len().checked_sub(1).map(NodeId::from_index)
    }

    fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> {
        self.edges
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or(&EMPTY_CHILDREN)
            .iter()
            .copied()
    }

    fn postorder(&self, start: NodeId) -> impl Iterator<Item = NodeId> {
        GenericDagPostorderIter {
            dag: self,
            start,
            next_index: 0,
        }
    }

    fn preorder(&self, start: NodeId) -> impl Iterator<Item = NodeId> {
        GenericDagPreorderIter {
            dag: self,
            start,
            next_ordinal: 0,
        }
    }
}

impl<N, L, A> MutDag for GenericDag<N, L, A> {
    fn add_node(&mut self, n: Self::Node) -> NodeId {
        let id = NodeId::from_index(self.nodes.len());
        self.nodes.push(n);
        id
    }

    fn add_edge(&mut self, from: NodeId, to: NodeId) {
        self.edges.entry(from).or_default().push(to);
    }

    fn set_leaf_data(&mut self, n: NodeId, d: Self::Leaf) {
        self.data.insert(n, d);
    }

    fn set_annotation(&mut self, n: NodeId, a: Self::Annotation) {
        self.annotations.insert(n, a);
    }
}
