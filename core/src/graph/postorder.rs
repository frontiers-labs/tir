use std::collections::HashMap;

use crate::{
    OpId, TypeId,
    graph::{Dag, EMPTY_CHILDREN, MutDag, NodeId},
};

/// A DAG where nodes are physically stored in strict post order.
pub struct PostOrderDag<N, L> {
    nodes: Vec<N>,
    edges: HashMap<NodeId, Vec<NodeId>>,
    parents: HashMap<NodeId, Vec<NodeId>>,
    data: HashMap<NodeId, L>,
    original_ops: HashMap<NodeId, OpId>,
    actual_types: HashMap<NodeId, TypeId>,
    descendants: Vec<Vec<u64>>,
}

impl<N, L> PostOrderDag<N, L> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: HashMap::new(),
            parents: HashMap::new(),
            data: HashMap::new(),
            original_ops: HashMap::new(),
            actual_types: HashMap::new(),
            descendants: Vec::new(),
        }
    }

    fn word_len(&self) -> usize {
        self.nodes.len().div_ceil(64)
    }

    fn mark_descendant(bits: &mut [u64], node: NodeId) {
        let idx = node.index();
        bits[idx / 64] |= 1u64 << (idx % 64);
    }

    fn contains_reachable(&self, root: NodeId, target: NodeId) -> bool {
        let idx = target.index();
        (self.descendants[root.index()][idx / 64] & (1u64 << (idx % 64))) != 0
    }

    fn merge_descendants(&mut self, into: NodeId, from: NodeId) -> bool {
        let src = self.descendants[from.index()].clone();
        self.merge_bits(into, &src)
    }

    fn merge_bits(&mut self, into: NodeId, bits: &[u64]) -> bool {
        let dst = &mut self.descendants[into.index()];
        let mut changed = false;

        for (dst_word, src_word) in dst.iter_mut().zip(bits.iter().copied()) {
            let merged = *dst_word | src_word;
            changed |= merged != *dst_word;
            *dst_word = merged;
        }

        changed
    }

    fn propagate_reachability(&mut self, node: NodeId) {
        let parents = self.parents.get(&node).cloned().unwrap_or_default();
        for parent in parents {
            if self.merge_descendants(parent, node) {
                self.propagate_reachability(parent);
            }
        }
    }

    /// Post-order traversal of the subtree reachable from `root`, like
    /// [`Dag::postorder`] but starting the scan at `root`'s lowest-indexed
    /// descendant instead of node 0. For a tree (every node has one parent) a
    /// subtree occupies a contiguous index range, so this visits exactly the
    /// subtree — `O(subtree)` rather than `O(root index)`.
    pub fn postorder_from(&self, root: NodeId) -> PostOrderDagIter<'_, N, L> {
        PostOrderDagIter {
            dag: self,
            root,
            next_index: self.first_descendant(root),
            end_index: root.index(),
        }
    }

    /// The smallest index reachable from `root`. `root` always marks itself, so
    /// the result never exceeds `root.index()`.
    fn first_descendant(&self, root: NodeId) -> usize {
        let bits = &self.descendants[root.index()];
        for (word, &mask) in bits.iter().enumerate() {
            if mask != 0 {
                return word * 64 + mask.trailing_zeros() as usize;
            }
        }
        root.index()
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

impl<N, L> Default for PostOrderDag<N, L> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct PostOrderDagIter<'a, N, L> {
    dag: &'a PostOrderDag<N, L>,
    root: NodeId,
    next_index: usize,
    end_index: usize,
}

impl<N, L> Iterator for PostOrderDagIter<'_, N, L> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_index <= self.end_index {
            let candidate = NodeId::from_index(self.next_index);
            self.next_index += 1;

            if self.dag.contains_reachable(self.root, candidate) {
                return Some(candidate);
            }
        }

        None
    }
}

pub struct PostOrderDagPreorderIter<'a, N, L> {
    dag: &'a PostOrderDag<N, L>,
    start: NodeId,
    next_ordinal: usize,
}

impl<N, L> Iterator for PostOrderDagPreorderIter<'_, N, L> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        let mut remaining = self.next_ordinal;
        let next = self.dag.nth_preorder(self.start, &mut remaining)?;
        self.next_ordinal += 1;
        Some(next)
    }
}

impl<N, L> Dag for PostOrderDag<N, L> {
    type Node = N;
    type Leaf = L;

    fn len(&self) -> usize {
        self.nodes.len()
    }

    fn get_node(&self, id: NodeId) -> &Self::Node {
        &self.nodes[id.index()]
    }

    fn get_leaf_data(&self, id: NodeId) -> Option<&Self::Leaf> {
        self.data.get(&id)
    }

    fn get_original_op(&self, id: NodeId) -> Option<OpId> {
        self.original_ops.get(&id).copied()
    }

    fn get_actual_type(&self, id: NodeId) -> Option<TypeId> {
        self.actual_types.get(&id).copied()
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
        PostOrderDagIter {
            dag: self,
            root: start,
            next_index: 0,
            end_index: start.index(),
        }
    }

    fn preorder(&self, start: NodeId) -> impl Iterator<Item = NodeId> {
        PostOrderDagPreorderIter {
            dag: self,
            start,
            next_ordinal: 0,
        }
    }
}

impl<N, L> MutDag for PostOrderDag<N, L> {
    fn add_node(&mut self, n: Self::Node) -> NodeId {
        let id = NodeId::from_index(self.nodes.len());
        self.nodes.push(n);

        let word_len = self.word_len();
        for bits in &mut self.descendants {
            bits.resize(word_len, 0);
        }

        let mut bits = vec![0; word_len];
        Self::mark_descendant(&mut bits, id);
        self.descendants.push(bits);

        id
    }

    fn add_edge(&mut self, from: NodeId, to: NodeId) {
        assert!(
            to.index() < from.index(),
            "PostOrderDag must ensure strict post order"
        );

        self.edges.entry(from).or_default().push(to);
        self.parents.entry(to).or_default().push(from);

        if self.merge_descendants(from, to) {
            self.propagate_reachability(from);
        }
    }

    fn set_leaf_data(&mut self, n: NodeId, d: Self::Leaf) {
        self.data.insert(n, d);
    }

    fn set_original_op(&mut self, n: NodeId, op: OpId) {
        self.original_ops.insert(n, op);
    }

    fn set_actual_type(&mut self, n: NodeId, ty: TypeId) {
        self.actual_types.insert(n, ty);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Dag;

    /// Build the tree `(0+1) + (3+4)` laid out in post order: leaves first, then
    /// each `Add`, then the root. Returns the dag and its root.
    fn sample() -> (PostOrderDag<&'static str, ()>, NodeId) {
        let mut dag = PostOrderDag::new();
        let a = dag.add_node("0");
        let b = dag.add_node("1");
        let left = dag.add_node("+");
        dag.add_edge(left, a);
        dag.add_edge(left, b);
        let c = dag.add_node("3");
        let d = dag.add_node("4");
        let right = dag.add_node("+");
        dag.add_edge(right, c);
        dag.add_edge(right, d);
        let root = dag.add_node("+");
        dag.add_edge(root, left);
        dag.add_edge(root, right);
        (dag, root)
    }

    #[test]
    fn postorder_from_matches_global_postorder_at_root() {
        let (dag, root) = sample();
        let global: Vec<_> = dag.postorder(root).collect();
        let from: Vec<_> = dag.postorder_from(root).collect();
        assert_eq!(global, from);
        assert_eq!(from.len(), dag.len());
    }

    #[test]
    fn postorder_from_visits_only_the_subtree() {
        let (dag, _root) = sample();
        // The right `Add` is node 5; its subtree is nodes 3, 4, 5.
        let right = NodeId::from_index(5);
        let nodes: Vec<_> = dag.postorder_from(right).collect();
        assert_eq!(
            nodes,
            vec![
                NodeId::from_index(3),
                NodeId::from_index(4),
                NodeId::from_index(5),
            ]
        );
    }
}
