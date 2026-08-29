use std::collections::HashMap;

use tir_adt::FxBuildHasher;
use tir_relational::Csr;

use crate::egraph::{EGraph, ENode, Id};

type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

/// No slot: a class the extraction's table does not cover.
const NONE: u32 = u32::MAX;

/// Cheapest representative e-node per e-class, chosen by [`EGraph::extract_best`].
pub struct Extraction<L: ENode> {
    best: FxHashMap<Id, L>,
}

impl<L: ENode> Extraction<L> {
    /// Chosen node for `id`'s class, or `None` if no node has finite cost. `id` must
    /// be canonical ([`EGraph::find`]).
    pub fn node(&self, id: Id) -> Option<&L> {
        self.best.get(&id)
    }
}

impl<L: ENode> EGraph<L> {
    /// Greedy bottom-up extraction: per class, the node minimizing
    /// `cost_of(class, node)` plus each child's chosen cost. The class is the
    /// canonical id the node would represent, so a cost model may reject a form
    /// the class cannot be spelled in. Cycle-tolerant — a node with un-costed
    /// children is skipped and revisited to a fixpoint, so a cycle is costed
    /// through its non-cyclic input. Scope-aware via
    /// [`EGraph::classes`]/[`EGraph::find`].
    pub fn extract_best(&self, cost_of: impl Fn(Id, &L) -> u64) -> Extraction<L> {
        let started = super::telemetry::enabled().then(std::time::Instant::now);
        let extraction = self.extract_best_inner(cost_of);
        if let Some(started) = started {
            super::telemetry::count_extract(started.elapsed());
        }
        extraction
    }

    fn extract_best_inner(&self, cost_of: impl Fn(Id, &L) -> u64) -> Extraction<L> {
        let graph = FlatGraph::new(self, cost_of);
        let mut cost: Vec<Option<u64>> = vec![None; graph.classes.len()];
        let mut best: Vec<Option<usize>> = vec![None; graph.classes.len()];

        // Rounds sweep the e-nodes in scan order — class order, then node order —
        // so which node wins a cost tie is the same as a full re-scan's. Only the
        // parents of a class improved in the previous sweep can improve in the next,
        // so `dirty` shrinks to the propagation frontier.
        let mut dirty = vec![true; graph.nodes.len()];
        let mut next = vec![false; graph.nodes.len()];
        let mut queued = graph.nodes.len();
        while queued > 0 {
            queued = 0;
            for position in 0..graph.nodes.len() {
                if !std::mem::replace(&mut dirty[position], false) {
                    continue;
                }
                let node = &graph.nodes[position];
                let Some(total) = graph.cost_of(node, &cost) else {
                    continue;
                };
                if cost[node.class].is_none_or(|best| total < best) {
                    cost[node.class] = Some(total);
                    best[node.class] = Some(position);
                    for &parent in graph.parents.get(node.class as u32) {
                        // A parent still ahead of the sweep sees the improvement in
                        // this round, exactly as a full re-scan would; an earlier one
                        // waits for the next.
                        let parent = parent as usize;
                        if parent > position {
                            dirty[parent] = true;
                        } else if !std::mem::replace(&mut next[parent], true) {
                            queued += 1;
                        }
                    }
                }
            }
            std::mem::swap(&mut dirty, &mut next);
            next.fill(false);
        }

        Extraction {
            best: graph
                .classes
                .iter()
                .zip(&best)
                .filter_map(|(&id, &position)| Some((id, graph.nodes[position?].node.clone())))
                .collect(),
        }
    }
}

/// The e-graph flattened for extraction: dense class slots and e-nodes in scan
/// order, with operator cost and child slots resolved once so the cost fixpoint
/// touches neither the union-find nor a hash map.
struct FlatGraph<'a, L: ENode> {
    /// Slot -> canonical class id.
    classes: Vec<Id>,
    nodes: Vec<FlatNode<'a, L>>,
    /// Class slot -> positions of the e-nodes taking it as a child, ascending.
    parents: Csr,
    /// Child slots, sliced by [`FlatNode::children`].
    children: Vec<usize>,
}

struct FlatNode<'a, L> {
    node: &'a L,
    class: usize,
    base: u64,
    children: std::ops::Range<usize>,
    /// A child outside the class table can never be costed, so neither can this node.
    costable: bool,
}

impl<'a, L: ENode> FlatGraph<'a, L> {
    fn new(eg: &'a EGraph<L>, cost_of: impl Fn(Id, &L) -> u64) -> Self {
        // Class id -> slot, as a dense array: ids run to the graph's high-water
        // mark, absorbed and scope-minted ones included, but the table is a
        // fraction of what a probe per child costs.
        let mut index: Vec<u32> = vec![NONE; eg.class_count()];
        let mut classes: Vec<Id> = Vec::new();
        let mut nodes: Vec<FlatNode<'a, L>> = Vec::new();
        let mut rows: Vec<tir_relational::RowId> = Vec::new();
        for id in eg.class_ids() {
            let slot = classes.len() as u32;
            index[id.index()] = slot;
            classes.push(id);
            for row in eg.rows(id) {
                let node = eg.node(row);
                rows.push(row);
                nodes.push(FlatNode {
                    node,
                    class: slot as usize,
                    base: cost_of(id, node),
                    children: 0..0,
                    costable: true,
                });
            }
        }

        let mut children = Vec::new();
        let mut edges: Vec<(u32, u32)> = Vec::new();
        for (position, entry) in nodes.iter_mut().enumerate() {
            let start = children.len();
            for &child in eg.children(rows[position]) {
                match index[eg.find(child).index()] {
                    NONE => entry.costable = false,
                    slot => {
                        children.push(slot as usize);
                        edges.push((slot, position as u32));
                    }
                }
            }
            entry.children = start..children.len();
        }

        Self {
            parents: Csr::build(classes.len(), edges),
            classes,
            nodes,
            children,
        }
    }

    /// The node's operator cost plus each child class's best cost, or `None` if any
    /// child is un-costed.
    fn cost_of(&self, node: &FlatNode<'a, L>, cost: &[Option<u64>]) -> Option<u64> {
        if !node.costable {
            return None;
        }
        let mut total = node.base;
        for &slot in &self.children[node.children.clone()] {
            total = total.saturating_add(cost[slot]?);
        }
        Some(total)
    }
}
