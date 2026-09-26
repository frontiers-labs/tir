//! An append-only DAG stored in topological order: every child precedes its
//! parent, so the last node is the root and a reverse index sweep visits
//! parents before children. A node's children sit in one contiguous edge run,
//! and the optional leaf payload and annotation live in dense side vectors that
//! only nodes carrying them occupy.

use std::ops::Range;

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

const NO_SLOT: u32 = u32::MAX;

pub struct Dag<N, L, A = ()> {
    nodes: Vec<N>,
    /// Children of node `n` are `edges[edge_bounds[n]..edge_bounds[n + 1]]`.
    edge_bounds: Vec<u32>,
    edges: Vec<NodeId>,
    leaf_slots: Vec<u32>,
    leaves: Vec<L>,
    annotation_slots: Vec<u32>,
    annotations: Vec<A>,
}

impl<N, L, A> Default for Dag<N, L, A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N, L, A> Dag<N, L, A> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edge_bounds: vec![0],
            edges: Vec::new(),
            leaf_slots: Vec::new(),
            leaves: Vec::new(),
            annotation_slots: Vec::new(),
            annotations: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The last node added, which every other node precedes.
    pub fn root(&self) -> Option<NodeId> {
        self.nodes.len().checked_sub(1).map(NodeId::from_index)
    }

    pub fn get_node(&self, id: NodeId) -> &N {
        &self.nodes[id.index()]
    }

    pub fn get_leaf_data(&self, id: NodeId) -> Option<&L> {
        slot(&self.leaf_slots, id).map(|slot| &self.leaves[slot])
    }

    pub fn get_annotation(&self, id: NodeId) -> Option<&A> {
        slot(&self.annotation_slots, id).map(|slot| &self.annotations[slot])
    }

    pub fn children(
        &self,
        id: NodeId,
    ) -> impl DoubleEndedIterator<Item = NodeId> + ExactSizeIterator + '_ {
        self.edges[self.edge_range(id)].iter().copied()
    }

    fn edge_range(&self, id: NodeId) -> Range<usize> {
        self.edge_bounds[id.index()] as usize..self.edge_bounds[id.index() + 1] as usize
    }

    /// Nodes reachable from `start`, each once, in depth-first preorder with
    /// children in edge order.
    pub fn preorder(&self, start: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut visited = vec![false; start.index() + 1];
        let mut stack = vec![start];
        std::iter::from_fn(move || {
            while let Some(node) = stack.pop() {
                if !std::mem::replace(&mut visited[node.index()], true) {
                    stack.extend(self.children(node).rev());
                    return Some(node);
                }
            }
            None
        })
    }

    /// Nodes reachable from `start` in ascending index order, which puts every
    /// child before its parents.
    pub fn postorder(&self, start: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let (low, reached) = self.reachable(start, 0);
        (low..=start.index())
            .filter(move |&index| reached[index - low])
            .map(NodeId::from_index)
    }

    /// Whether `to` is `from` or one of its descendants.
    pub fn reaches(&self, from: NodeId, to: NodeId) -> bool {
        if to.index() > from.index() {
            return false;
        }
        let (low, reached) = self.reachable(from, to.index());
        low == to.index() && reached[0]
    }

    /// Marks the nodes at or above index `floor` that `start` reaches, sweeping
    /// down from `start` until no marked node is left unexpanded. Returns the
    /// lowest marked index and the marks from it up to `start`.
    fn reachable(&self, start: NodeId, floor: usize) -> (usize, Vec<bool>) {
        let mut reached = vec![false; start.index() + 1 - floor];
        reached[start.index() - floor] = true;
        let mut pending = 1usize;
        let mut low = start.index();
        for index in (floor..=start.index()).rev() {
            if pending == 0 {
                break;
            }
            if !reached[index - floor] {
                continue;
            }
            pending -= 1;
            low = index;
            for child in self.children(NodeId::from_index(index)) {
                let Some(mark) = child.index().checked_sub(floor) else {
                    continue;
                };
                if !std::mem::replace(&mut reached[mark], true) {
                    pending += 1;
                }
            }
        }
        reached.drain(..low - floor);
        (low, reached)
    }

    /// Whether the subgraphs rooted at `root` and `other_root` match node for
    /// node: same labels, leaf payloads and children, in order.
    pub fn subgraph_eq<B>(&self, root: NodeId, other: &Dag<N, L, B>, other_root: NodeId) -> bool
    where
        N: PartialEq,
        L: PartialEq,
    {
        let mut pending = vec![(root, other_root)];
        while let Some((lhs, rhs)) = pending.pop() {
            let lhs_children = &self.edges[self.edge_range(lhs)];
            let rhs_children = &other.edges[other.edge_range(rhs)];
            if self.get_node(lhs) != other.get_node(rhs)
                || self.get_leaf_data(lhs) != other.get_leaf_data(rhs)
                || lhs_children.len() != rhs_children.len()
            {
                return false;
            }
            pending.extend(
                lhs_children
                    .iter()
                    .copied()
                    .zip(rhs_children.iter().copied()),
            );
        }
        true
    }

    pub fn add_node(&mut self, n: N) -> NodeId {
        let id = NodeId::from_index(self.nodes.len());
        self.nodes.push(n);
        self.edge_bounds.push(self.edges.len() as u32);
        self.leaf_slots.push(NO_SLOT);
        self.annotation_slots.push(NO_SLOT);
        id
    }

    /// Appends `to` to the children of `from`, which must be the node added
    /// last so its edges stay contiguous.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId) {
        assert_eq!(
            from.index() + 1,
            self.nodes.len(),
            "edges must be added to the newest node"
        );
        assert!(to.index() < from.index(), "a child must precede its parent");
        self.edges.push(to);
        *self.edge_bounds.last_mut().unwrap() += 1;
    }

    pub fn set_leaf_data(&mut self, n: NodeId, d: L) {
        set_slot(&mut self.leaf_slots, &mut self.leaves, n, d);
    }

    pub fn set_annotation(&mut self, n: NodeId, a: A) {
        set_slot(&mut self.annotation_slots, &mut self.annotations, n, a);
    }

    /// The annotation of `n`, attaching a default one first if it has none.
    pub fn annotation_mut(&mut self, n: NodeId) -> &mut A
    where
        A: Default,
    {
        let slot = match slot(&self.annotation_slots, n) {
            Some(slot) => slot,
            None => {
                self.annotation_slots[n.index()] = self.annotations.len() as u32;
                self.annotations.push(A::default());
                self.annotations.len() - 1
            }
        };
        &mut self.annotations[slot]
    }
}

fn slot(slots: &[u32], id: NodeId) -> Option<usize> {
    let slot = slots[id.index()];
    (slot != NO_SLOT).then_some(slot as usize)
}

fn set_slot<T>(slots: &mut [u32], values: &mut Vec<T>, id: NodeId, value: T) {
    match slot(slots, id) {
        Some(slot) => values[slot] = value,
        None => {
            slots[id.index()] = values.len() as u32;
            values.push(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `unrelated` precedes a diamond whose `shared` leaf both `lhs` and `rhs`
    /// read: node 0 unrelated, 1 shared, 2 lhs, 3 rhs, 4 root.
    fn diamond() -> Dag<&'static str, ()> {
        let mut dag = Dag::new();
        dag.add_node("unrelated");
        let shared = dag.add_node("shared");
        let lhs = dag.add_node("lhs");
        dag.add_edge(lhs, shared);
        let rhs = dag.add_node("rhs");
        dag.add_edge(rhs, shared);
        let root = dag.add_node("root");
        dag.add_edge(root, lhs);
        dag.add_edge(root, rhs);
        dag
    }

    fn indices(nodes: impl Iterator<Item = NodeId>) -> Vec<usize> {
        nodes.map(NodeId::index).collect()
    }

    #[test]
    fn traversals_visit_reachable_nodes_once() {
        let dag = diamond();
        let root = dag.root().unwrap();
        assert_eq!(indices(dag.preorder(root)), [4, 2, 1, 3]);
        assert_eq!(indices(dag.postorder(root)), [1, 2, 3, 4]);
        assert_eq!(indices(dag.postorder(NodeId::from_index(3))), [1, 3]);
    }

    #[test]
    fn reaches_follows_edges_only() {
        let dag = diamond();
        let root = dag.root().unwrap();
        assert!(dag.reaches(root, NodeId::from_index(1)));
        assert!(!dag.reaches(root, NodeId::from_index(0)));
        assert!(!dag.reaches(NodeId::from_index(2), NodeId::from_index(3)));
    }

    #[test]
    #[should_panic(expected = "newest node")]
    fn edges_attach_to_the_newest_node() {
        let mut dag: Dag<&str, ()> = Dag::new();
        let parent = dag.add_node("parent");
        let child = dag.add_node("child");
        dag.add_edge(parent, child);
    }
}
