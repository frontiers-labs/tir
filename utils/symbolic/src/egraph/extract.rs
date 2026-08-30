use std::collections::HashMap;

use tir_adt::FxBuildHasher;
use tir_relational::Csr;

use crate::egraph::{EGraph, ENode, Id};

type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

/// No slot: a class the extraction's table does not cover.
const NONE: u32 = u32::MAX;

/// Cheapest representative e-node per e-class, chosen by [`EGraph::extract_best`].
///
/// A scope's extraction is a layer over the one it refreshed rather than a copy
/// of it, so it holds only the classes its assumption dirtied and costs the
/// dirty set instead of the graph.
pub struct Extraction<'a, L: ENode> {
    base: Option<&'a Extraction<'a, L>>,
    /// What this layer settled. An explicit `None` is a class the layer
    /// recomputed and found no finite cost for; it must read as nothing rather
    /// than fall through to the base's stale answer.
    best: FxHashMap<Id, Option<Chosen<L>>>,
}

/// What a class was extracted as, and what that spelling cost. The cost is what
/// an incremental re-extraction reads for a class it does not recompute.
#[derive(Clone)]
struct Chosen<L> {
    node: L,
    cost: u64,
}

impl<'a, L: ENode> Extraction<'a, L> {
    /// Chosen node for `id`'s class, or `None` if no node has finite cost. `id` must
    /// be canonical ([`EGraph::find`]).
    pub fn node(&self, id: Id) -> Option<&L> {
        self.chosen(id).map(|chosen| &chosen.node)
    }

    /// This extraction with the classes in `dirty`, and only those, recomputed
    /// against `eg` as it stands now.
    ///
    /// A class's answer depends on its own rows and its children's costs. `dirty`
    /// must therefore be closed upward over parents, which is what
    /// [`tir_relational::Engine::scope_dirty`] returns: every class an open scope
    /// changed the rows or the children of. A class outside it has the rows and
    /// the child costs it had when `self` was taken, so it keeps its answer.
    /// Ascending order makes the sweep the same scan a full pass takes over those
    /// classes, which is what keeps the cost ties broken the same way.
    pub fn refresh<'b>(
        &'b self,
        eg: &EGraph<L>,
        dirty: &[Id],
        cost_of: impl Fn(Id, &L) -> u64,
    ) -> Extraction<'b, L> {
        let started = super::telemetry::enabled().then(std::time::Instant::now);
        let extraction = FlatGraph::new(eg, dirty, Some(self), cost_of).solve(Some(self));
        if let Some(started) = started {
            super::telemetry::count_extract(started.elapsed());
        }
        extraction
    }

    fn cost(&self, id: Id) -> Option<u64> {
        self.chosen(id).map(|chosen| chosen.cost)
    }

    /// What the innermost layer holding `id` settled on, `None` once a layer
    /// says the class has no finite cost.
    fn chosen(&self, id: Id) -> Option<&Chosen<L>> {
        match self.best.get(&id) {
            Some(settled) => settled.as_ref(),
            None => self.base?.chosen(id),
        }
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
    pub fn extract_best(&self, cost_of: impl Fn(Id, &L) -> u64) -> Extraction<'static, L> {
        let started = super::telemetry::enabled().then(std::time::Instant::now);
        let classes: Vec<Id> = self.class_ids().collect();
        let extraction = FlatGraph::new(self, &classes, None, cost_of).solve(None);
        if let Some(started) = started {
            super::telemetry::count_extract(started.elapsed());
        }
        extraction
    }
}

/// The e-graph flattened for extraction: dense class slots and e-nodes in scan
/// order, with operator cost and child slots resolved once so the cost fixpoint
/// touches neither the union-find nor a hash map.
///
/// The table covers the classes it was built over, which is every class for a
/// full extraction and the dirty set for a scope's. A child outside the table is
/// costed from `outside`, the extraction the scope refreshes, and is un-costable
/// without one.
struct FlatGraph<'a, L: ENode> {
    /// Slot -> canonical class id.
    classes: &'a [Id],
    nodes: Vec<FlatNode<'a, L>>,
    /// Class slot -> positions of the e-nodes taking it as a child, ascending.
    parents: Csr,
    /// Child slots, sliced by [`FlatNode::children`].
    children: Vec<usize>,
}

struct FlatNode<'a, L> {
    node: &'a L,
    class: usize,
    /// Operator cost, plus the settled cost of every child outside the table.
    base: u64,
    children: std::ops::Range<usize>,
    /// A child outside the table that nothing costs can never be costed, so
    /// neither can this node.
    costable: bool,
}

impl<'a, L: ENode> FlatGraph<'a, L> {
    fn new(
        eg: &'a EGraph<L>,
        classes: &'a [Id],
        outside: Option<&Extraction<'_, L>>,
        cost_of: impl Fn(Id, &L) -> u64,
    ) -> Self {
        // Class id -> slot, as a dense array: ids run to the graph's high-water
        // mark, absorbed and scope-minted ones included, but the table is a
        // fraction of what a probe per child costs.
        let mut index: Vec<u32> = vec![NONE; eg.class_count()];
        let mut nodes: Vec<FlatNode<'a, L>> = Vec::new();
        let mut rows: Vec<tir_relational::RowId> = Vec::new();
        for (slot, &id) in classes.iter().enumerate() {
            index[id.index()] = slot as u32;
            for row in eg.rows(id) {
                let node = eg.node(row);
                rows.push(row);
                nodes.push(FlatNode {
                    node,
                    class: slot,
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
                let child = eg.find(child);
                match index[child.index()] {
                    NONE => match outside.and_then(|base| base.cost(child)) {
                        Some(cost) => entry.base = entry.base.saturating_add(cost),
                        None => entry.costable = false,
                    },
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

    /// Run the cost fixpoint over the table and record its winners as a layer
    /// over `base`.
    fn solve<'b>(&self, base: Option<&'b Extraction<'b, L>>) -> Extraction<'b, L> {
        let cost = self.costs();

        // The winner is the first node in scan order — class order, then node
        // order — that spells its class at the settled cost. Reading it off the
        // costs rather than off the fixpoint's improvement order is what makes a
        // tie break the same way whatever schedule reached those costs, which is
        // what [`Extraction::refresh`] needs: it starts from the costs a base
        // extraction settled, so it converges in a different order than a full
        // pass over the same graph.
        let mut chosen: Vec<Option<usize>> = vec![None; self.classes.len()];
        for (position, node) in self.nodes.iter().enumerate() {
            if cost[node.class].is_some()
                && chosen[node.class].is_none()
                && self.cost_of(node, &cost) == cost[node.class]
            {
                chosen[node.class] = Some(position);
            }
        }

        let mut best = FxHashMap::default();
        for (slot, &id) in self.classes.iter().enumerate() {
            match chosen[slot] {
                Some(position) => {
                    best.insert(
                        id,
                        Some(Chosen {
                            node: self.nodes[position].node.clone(),
                            cost: cost[slot].expect("a chosen node is a costed one"),
                        }),
                    );
                }
                // Only a layer has a base to shadow; the bottom one says nothing
                // about a class it could not cost.
                None if base.is_some() => {
                    best.insert(id, None);
                }
                None => {}
            }
        }
        Extraction { base, best }
    }

    /// Least cost per class slot, or `None` where no node has a finite one.
    ///
    /// Only the parents of a class improved in the previous sweep can improve in
    /// the next, so `dirty` shrinks to the propagation frontier.
    fn costs(&self) -> Vec<Option<u64>> {
        let mut cost: Vec<Option<u64>> = vec![None; self.classes.len()];
        let mut dirty = vec![true; self.nodes.len()];
        let mut next = vec![false; self.nodes.len()];
        let mut queued = self.nodes.len();
        while queued > 0 {
            queued = 0;
            for position in 0..self.nodes.len() {
                if !std::mem::replace(&mut dirty[position], false) {
                    continue;
                }
                let node = &self.nodes[position];
                let Some(total) = self.cost_of(node, &cost) else {
                    continue;
                };
                if cost[node.class].is_none_or(|best| total < best) {
                    cost[node.class] = Some(total);
                    for &parent in self.parents.get(node.class as u32) {
                        // A parent still ahead of the sweep sees the improvement
                        // in this round; an earlier one waits for the next.
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
        cost
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
