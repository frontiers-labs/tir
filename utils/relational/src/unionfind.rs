use crate::ClassId;

/// One partition over the class ids: parent pointers plus the set size at a root.
#[derive(Clone, Default)]
struct Level {
    parent: Vec<u32>,
    size: Vec<u32>,
}

impl Level {
    fn with_size(len: usize) -> Self {
        Self {
            parent: (0..len as u32).collect(),
            size: vec![1; len],
        }
    }

    fn push(&mut self) {
        self.parent.push(self.parent.len() as u32);
        self.size.push(1);
    }

    fn find(&self, mut cur: u32) -> u32 {
        while self.parent[cur as usize] != cur {
            cur = self.parent[cur as usize];
        }
        cur
    }

    /// Merge two roots, returning the survivor: the larger set's root, ties
    /// going to `b`.
    fn union(&mut self, mut a: u32, mut b: u32) -> u32 {
        if self.size[a as usize] > self.size[b as usize] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[a as usize] = b;
        self.size[b as usize] += self.size[a as usize];
        b
    }

    /// Pointer-jump every entry to its root, so the next round's finds are one
    /// load: rounds of `parent[i] = parent[parent[i]]` to a fixpoint.
    fn flatten(&mut self) {
        let mut moved = true;
        while moved {
            moved = false;
            for i in 0..self.parent.len() {
                let p = self.parent[i] as usize;
                let grand = self.parent[p];
                if grand != self.parent[i] {
                    self.parent[i] = grand;
                    moved = true;
                }
            }
        }
    }
}

/// Union-find over class ids with stack-disciplined assumption scopes.
///
/// A scope is a fresh partition layered over the base one, so a pop discards its
/// merges by dropping the layer — no undo log, and no way for a hypothesis to
/// leave a trace in the base. The layer starts as singletons, so the size that
/// breaks a scoped union's tie counts only what the scope itself merged: the
/// survivor of a scoped union is *not* the one the same union would pick in the
/// base graph, and everything keyed by canonical ids depends on that.
///
/// Unions hook immediately — a caller that merges two classes sees the survivor
/// from the next [`Self::find`], and an applier that instantiates afterwards
/// hash-conses against it. Nothing is compressed until [`Self::flatten`] at a
/// rebuild, so a round's finds walk at most the depth its own unions built.
#[derive(Clone, Default)]
pub struct UnionFind {
    base: Level,
    layers: Vec<Level>,
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
        let mut root = self.base.find(id.0);
        for layer in &self.layers {
            root = layer.find(root);
        }
        ClassId(root)
    }

    /// Merge the classes of `a` and `b` in the innermost open scope (the base
    /// with none open), returning the survivor.
    pub fn union(&mut self, a: ClassId, b: ClassId) -> ClassId {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        let level = self.layers.last_mut().unwrap_or(&mut self.base);
        ClassId(level.union(ra.0, rb.0))
    }

    /// Enter an assumption scope; unions until the matching [`Self::pop_scope`]
    /// are local to it.
    pub fn push_scope(&mut self) {
        self.layers.push(Level::with_size(self.base.parent.len()));
    }

    /// Leave the scope, discarding its merges.
    pub fn pop_scope(&mut self) {
        self.layers.pop();
    }

    /// Flatten the partition the next round's finds read: the innermost open
    /// layer, or the base with none open.
    pub fn flatten(&mut self) {
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
    fn union_survivor_is_the_larger_set() {
        let mut uf = seeded(4);
        assert_eq!(uf.union(ClassId(0), ClassId(1)), ClassId(1));
        assert_eq!(uf.union(ClassId(2), ClassId(3)), ClassId(3));
        // {0,1} and {2,3} tie at size two, so the second argument's root wins.
        assert_eq!(uf.union(ClassId(0), ClassId(2)), ClassId(3));
        assert_eq!(uf.find(ClassId(0)), ClassId(3));
    }

    #[test]
    fn a_scope_weighs_only_its_own_merges() {
        let mut uf = seeded(3);
        uf.union(ClassId(0), ClassId(1));
        uf.push_scope();
        // In the base, the survivor's set has two members and would win; the
        // layer sees both sides as singletons, so the tie goes to `b`.
        assert_eq!(uf.union(ClassId(0), ClassId(2)), ClassId(2));
        uf.pop_scope();
        assert_eq!(uf.union(ClassId(0), ClassId(2)), ClassId(1));
    }

    #[test]
    fn a_scope_pop_discards_its_merges() {
        let mut uf = seeded(4);
        uf.push_scope();
        uf.union(ClassId(0), ClassId(1));
        uf.push_scope();
        uf.union(ClassId(1), ClassId(2));
        assert_eq!(uf.find(ClassId(0)), uf.find(ClassId(2)));
        uf.pop_scope();
        assert_eq!(uf.find(ClassId(0)), ClassId(1));
        assert_ne!(uf.find(ClassId(2)), ClassId(1));
        uf.pop_scope();
        assert_eq!(uf.find(ClassId(0)), ClassId(0));
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
            let mut size = [1u32; 16];
            for (a, b) in pairs {
                uf.union(ClassId(a), ClassId(b));
                let mut ra = reference_find(&parent, a);
                let mut rb = reference_find(&parent, b);
                if ra != rb {
                    if size[ra as usize] > size[rb as usize] {
                        std::mem::swap(&mut ra, &mut rb);
                    }
                    parent[ra as usize] = rb;
                    size[rb as usize] += size[ra as usize];
                }
            }
            for i in 0..16u32 {
                prop_assert_eq!(uf.find(ClassId(i)).0, reference_find(&parent, i));
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
            // Sizes have to come back too, or the next tie breaks the other way.
            let mut replay = seeded(12);
            for i in 0..12u32 {
                replay.union(ClassId(i), ClassId(roots[i as usize]));
            }
            prop_assert_eq!(uf.union(ClassId(0), ClassId(1)), replay.union(ClassId(0), ClassId(1)));
        }
    }
}
