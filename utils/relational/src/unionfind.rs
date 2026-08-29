use crate::ClassId;

/// Flat union-find over class ids.
///
/// Unions hook immediately — a caller that merges two classes sees the survivor
/// from the next [`Self::find`], as the tree-walking engine did, and an applier
/// that instantiates after one hash-conses against it — but no id is compressed
/// until [`Self::flatten`] rewrites the array to depth one at rebuild. A round's
/// finds therefore walk at most the depth its own unions built.
///
/// Every merge is logged in application order. A rebuild reads the log to learn
/// which classes were absorbed without scanning the array, and a scope
/// [`rolls it back`](Self::rollback) to undo its hypothesis.
#[derive(Clone, Default)]
pub struct UnionFind {
    parent: Vec<u32>,
    /// Set size at a root; meaningless elsewhere, and frozen from the moment a
    /// root is absorbed — which is what lets [`Self::rollback`] subtract it back.
    size: Vec<u32>,
    /// `(absorbed root, survivor)` per merge, in the order they were applied.
    log: Vec<(ClassId, ClassId)>,
}

impl UnionFind {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh singleton class.
    pub fn push(&mut self) -> ClassId {
        let id = self.parent.len() as u32;
        self.parent.push(id);
        self.size.push(1);
        ClassId(id)
    }

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    pub fn find(&self, id: ClassId) -> ClassId {
        let mut cur = id.0;
        while self.parent[cur as usize] != cur {
            cur = self.parent[cur as usize];
        }
        ClassId(cur)
    }

    /// Merge two classes, returning the survivor. The larger set's root wins,
    /// ties going to `b` — the rule the scalar engine used, kept so canonical
    /// ids, and every extraction tie-break and side-table key downstream of
    /// them, stay where they are.
    pub fn union(&mut self, a: ClassId, b: ClassId) -> ClassId {
        let mut ra = self.find(a);
        let mut rb = self.find(b);
        if ra == rb {
            return ra;
        }
        if self.size[ra.index()] > self.size[rb.index()] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[ra.index()] = rb.0;
        self.size[rb.index()] += self.size[ra.index()];
        self.log.push((ra, rb));
        rb
    }

    /// The merges since the last drain, in application order.
    pub fn take_log(&mut self) -> Vec<(ClassId, ClassId)> {
        std::mem::take(&mut self.log)
    }

    pub fn log_len(&self) -> usize {
        self.log.len()
    }

    /// Pointer-jump every entry to its root, so the next round's finds are one
    /// load. Rounds of `parent[i] = parent[parent[i]]` over the flat array, run
    /// to a fixpoint. Never called with a scope open: it would erase the
    /// structure [`Self::rollback`] walks back.
    pub fn flatten(&mut self) {
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

    /// Undo every merge logged after `mark` and drop the classes pushed after
    /// `classes`, restoring exactly the state the scope opened on. Merges are
    /// undone newest first: a root's size stops moving the moment it is
    /// absorbed, so subtracting it from its survivor restores that survivor.
    pub fn rollback(&mut self, mark: usize, classes: usize) {
        while self.log.len() > mark {
            let (absorbed, survivor) = self.log.pop().expect("logged merge");
            self.parent[absorbed.index()] = absorbed.0;
            self.size[survivor.index()] -= self.size[absorbed.index()];
        }
        self.parent.truncate(classes);
        self.size.truncate(classes);
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
    fn take_log_reports_every_merge_once() {
        let mut uf = seeded(3);
        uf.union(ClassId(0), ClassId(1));
        uf.union(ClassId(0), ClassId(1));
        uf.union(ClassId(1), ClassId(2));
        assert_eq!(
            uf.take_log(),
            vec![(ClassId(0), ClassId(1)), (ClassId(2), ClassId(1))]
        );
        assert!(uf.take_log().is_empty());
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
        fn rollback_restores_the_state_a_scope_opened_on(
            before in prop::collection::vec((0u32..12, 0u32..12), 0..24),
            inside in prop::collection::vec((0u32..12, 0u32..12), 0..24),
        ) {
            let mut uf = seeded(12);
            for (a, b) in before {
                uf.union(ClassId(a), ClassId(b));
            }
            let roots: Vec<u32> = (0..12).map(|i| uf.find(ClassId(i)).0).collect();
            let mark = uf.log_len();
            let classes = uf.len();
            for (a, b) in inside {
                uf.union(ClassId(a), ClassId(b));
            }
            uf.push();
            uf.rollback(mark, classes);
            prop_assert_eq!(uf.len(), classes);
            prop_assert_eq!(uf.log_len(), mark);
            for i in 0..12u32 {
                prop_assert_eq!(uf.find(ClassId(i)).0, roots[i as usize]);
            }
            // Sizes must come back too, or the next tie breaks the other way.
            let mut replay = seeded(12);
            for i in 0..12u32 {
                replay.union(ClassId(i), ClassId(roots[i as usize]));
            }
            prop_assert_eq!(uf.union(ClassId(0), ClassId(1)), replay.union(ClassId(0), ClassId(1)));
        }
    }
}
