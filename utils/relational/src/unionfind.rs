use std::cell::Cell;

use crate::ClassId;

/// One partition over the class ids.
///
/// The survivor of a merge is the smaller id, so a set's representative is the
/// minimum of its members — a function of the set, not of the order the merges
/// arrived in. That is what lets congruence repair visit collisions in whatever
/// order is fastest to produce rather than in the order a worklist happened to
/// queue them, which is the whole argument for the bulk rebuild.
///
/// It also gives `parent[i] <= i` invariantly, so flattening is one ascending
/// sweep instead of pointer-jumping rounds.
#[derive(Clone, Default)]
struct Level {
    parent: Vec<u32>,
}

impl Level {
    fn with_size(len: usize) -> Self {
        Self {
            parent: (0..len as u32).collect(),
        }
    }

    fn push(&mut self) {
        self.parent.push(self.parent.len() as u32);
    }

    fn find(&self, mut cur: u32) -> u32 {
        while self.parent[cur as usize] != cur {
            cur = self.parent[cur as usize];
        }
        cur
    }

    /// Merge two roots, returning the survivor: the smaller id.
    fn union(&mut self, a: u32, b: u32) -> u32 {
        let (survivor, absorbed) = if a < b { (a, b) } else { (b, a) };
        self.parent[absorbed as usize] = survivor;
        survivor
    }

    /// Rewrite every entry to its root. `parent[i] <= i`, so one ascending pass
    /// resolves the whole array: `parent[parent[i]]` is already a root by the
    /// time `i` is read.
    fn flatten(&mut self) {
        for i in 0..self.parent.len() {
            self.parent[i] = self.parent[self.parent[i] as usize];
        }
    }
}

/// Union-find over class ids with stack-disciplined assumption scopes.
///
/// A scope is a fresh partition layered over the base one, so a pop discards its
/// merges by dropping the layer — no undo log, and no way for a hypothesis to
/// leave a trace in the base.
///
/// Unions hook immediately — a caller that merges two classes sees the survivor
/// from the next [`Self::find`], and an applier that instantiates afterwards
/// hash-conses against it. Nothing is compressed until [`Self::flatten`] at a
/// rebuild, so a round's finds walk at most the depth its own unions built.
#[derive(Clone, Default)]
pub struct UnionFind {
    base: Level,
    layers: Vec<Level>,
    /// Each id's last answer, stamped with the epoch it was found in. A union or
    /// a scope boundary bumps the epoch, so the cache is dropped rather than
    /// unwound — path compression across layers would need an undo log, and a
    /// search phase performs no unions at all, so the cache holds for all of it.
    hint: Vec<Cell<(u32, u32)>>,
    epoch: Cell<u32>,
}

impl UnionFind {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh singleton class; the base and every open layer grow in lockstep.
    pub fn push(&mut self) -> ClassId {
        let id = self.base.parent.len() as u32;
        self.base.push();
        for layer in &mut self.layers {
            layer.push();
        }
        self.hint.push(Cell::new((id, 0)));
        ClassId(id)
    }

    pub fn len(&self) -> usize {
        self.base.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.base.parent.is_empty()
    }

    /// Canonicalize bottom-up: the base, then each open layer in order. A
    /// top-down walk would miss a layer's redirect for an id that layer never
    /// touched.
    pub fn find(&self, id: ClassId) -> ClassId {
        let epoch = self.epoch.get();
        let cached = self.hint[id.index()].get();
        if cached.1 == epoch {
            return ClassId(cached.0);
        }
        let mut root = self.base.find(id.0);
        for layer in &self.layers {
            root = layer.find(root);
        }
        self.hint[id.index()].set((root, epoch));
        ClassId(root)
    }

    /// Every stamp of the current epoch becomes stale. Wrapping onto an epoch a
    /// stamp still holds would resurrect it, so the wrap clears them instead.
    fn invalidate(&mut self) {
        let next = self.epoch.get().wrapping_add(1);
        if next == 0 {
            for slot in &self.hint {
                slot.set((0, 0));
            }
            self.epoch.set(1);
        } else {
            self.epoch.set(next);
        }
    }

    /// Merge the classes of `a` and `b` in the innermost open scope (the base
    /// with none open), returning the survivor.
    pub fn union(&mut self, a: ClassId, b: ClassId) -> ClassId {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        self.invalidate();
        let level = self.layers.last_mut().unwrap_or(&mut self.base);
        ClassId(level.union(ra.0, rb.0))
    }

    /// Enter an assumption scope; unions until the matching [`Self::pop_scope`]
    /// are local to it.
    pub fn push_scope(&mut self) {
        self.invalidate();
        self.layers.push(Level::with_size(self.base.parent.len()));
    }

    /// Leave the scope, discarding its merges.
    pub fn pop_scope(&mut self) {
        self.invalidate();
        self.layers.pop();
    }

    /// Flatten the partition the next round's finds read: the innermost open
    /// layer, or the base with none open.
    pub fn flatten(&mut self) {
        self.invalidate();
        self.layers.last_mut().unwrap_or(&mut self.base).flatten();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn reference_find(parent: &[u32], mut i: u32) -> u32 {
        while parent[i as usize] != i {
            i = parent[i as usize];
        }
        i
    }

    fn seeded(classes: u32) -> UnionFind {
        let mut uf = UnionFind::new();
        for _ in 0..classes {
            uf.push();
        }
        uf
    }

    #[test]
    fn a_set_is_represented_by_its_smallest_member() {
        let mut uf = seeded(4);
        assert_eq!(uf.union(ClassId(3), ClassId(1)), ClassId(1));
        assert_eq!(uf.union(ClassId(2), ClassId(0)), ClassId(0));
        assert_eq!(uf.union(ClassId(3), ClassId(2)), ClassId(0));
        for i in 0..4 {
            assert_eq!(uf.find(ClassId(i)), ClassId(0));
        }
    }

    #[test]
    fn the_representative_does_not_depend_on_merge_order() {
        let orders: [[(u32, u32); 3]; 2] = [[(3, 1), (2, 0), (3, 2)], [(2, 3), (0, 1), (1, 2)]];
        let roots: Vec<Vec<u32>> = orders
            .iter()
            .map(|order| {
                let mut uf = seeded(4);
                for &(a, b) in order {
                    uf.union(ClassId(a), ClassId(b));
                }
                (0..4).map(|i| uf.find(ClassId(i)).0).collect()
            })
            .collect();
        assert_eq!(roots[0], roots[1]);
    }

    #[test]
    fn a_scope_pop_discards_its_merges() {
        let mut uf = seeded(4);
        uf.push_scope();
        uf.union(ClassId(1), ClassId(2));
        uf.push_scope();
        uf.union(ClassId(2), ClassId(3));
        assert_eq!(uf.find(ClassId(1)), uf.find(ClassId(3)));
        uf.pop_scope();
        assert_eq!(uf.find(ClassId(2)), ClassId(1));
        assert_ne!(uf.find(ClassId(3)), ClassId(1));
        uf.pop_scope();
        assert_eq!(uf.find(ClassId(2)), ClassId(2));
    }

    #[test]
    fn a_class_minted_in_a_scope_survives_the_pop() {
        let mut uf = seeded(1);
        uf.push_scope();
        let minted = uf.push();
        uf.union(ClassId(0), minted);
        uf.pop_scope();
        assert_eq!(uf.find(minted), minted);
    }

    proptest! {
        #[test]
        fn flatten_agrees_with_a_walking_find(pairs in prop::collection::vec((0u32..16, 0u32..16), 0..64)) {
            let mut uf = seeded(16);
            for (a, b) in pairs {
                uf.union(ClassId(a), ClassId(b));
            }
            let before: Vec<u32> = (0..16).map(|i| uf.find(ClassId(i)).0).collect();
            uf.flatten();
            for i in 0..16u32 {
                prop_assert_eq!(uf.find(ClassId(i)).0, before[i as usize]);
            }
        }

        #[test]
        fn find_equals_the_reference_walk(pairs in prop::collection::vec((0u32..16, 0u32..16), 0..64)) {
            let mut uf = seeded(16);
            let mut parent: Vec<u32> = (0..16).collect();
            for (a, b) in pairs {
                uf.union(ClassId(a), ClassId(b));
                let ra = reference_find(&parent, a);
                let rb = reference_find(&parent, b);
                parent[ra.max(rb) as usize] = ra.min(rb);
            }
            for i in 0..16u32 {
                prop_assert_eq!(uf.find(ClassId(i)).0, reference_find(&parent, i));
            }
        }

        /// The representative is the minimum of the set, however the merges that
        /// built it were ordered — which is what a rebuild that visits
        /// collisions in sorted-key order relies on.
        #[test]
        fn a_representative_is_the_minimum_of_its_set(pairs in prop::collection::vec((0u32..16, 0u32..16), 0..64)) {
            let mut uf = seeded(16);
            for (a, b) in pairs {
                uf.union(ClassId(a), ClassId(b));
            }
            for i in 0..16u32 {
                let root = uf.find(ClassId(i));
                let members = (0..16u32).filter(|&j| uf.find(ClassId(j)) == root);
                prop_assert_eq!(root.0, members.min().expect("a set holds itself"));
            }
        }

        #[test]
        fn a_scope_round_trip_restores_the_partition(
            before in prop::collection::vec((0u32..12, 0u32..12), 0..24),
            inside in prop::collection::vec((0u32..12, 0u32..12), 0..24),
        ) {
            let mut uf = seeded(12);
            for (a, b) in before {
                uf.union(ClassId(a), ClassId(b));
            }
            let roots: Vec<u32> = (0..12).map(|i| uf.find(ClassId(i)).0).collect();
            uf.push_scope();
            for (a, b) in inside {
                uf.union(ClassId(a), ClassId(b));
            }
            uf.push();
            uf.pop_scope();
            for i in 0..12u32 {
                prop_assert_eq!(uf.find(ClassId(i)).0, roots[i as usize]);
            }
        }
    }
}
