use crate::Ref;

/// The mask of a carrier `width` bits wide: offsets on it live modulo
/// `mask + 1`. Zero for an id with no carrier, whose offsets are all zero.
pub fn mask(width: u8) -> u64 {
    match width {
        0 => 0,
        width => u64::MAX >> (64 - u32::from(width.min(64))),
    }
}

/// What a [`UnionFind::union`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    /// The two references were already equal.
    Same,
    Merged {
        survivor: u32,
        absorbed: u32,
    },
    /// The two references name one id at different offsets, or two
    /// carriers' zeros: nothing was changed.
    Contradiction,
}

/// One partition over the ids of a space, with each id placed at an offset
/// from its parent: `id = parent + delta`, modulo the carrier's width.
///
/// The survivor of a merge is the smaller id, unless one side is pinned (a
/// carrier's zero), which always survives: "this id is the constant `k`"
/// then reads as "its representative is a zero, at offset `k`". Either way a
/// set's representative is a function of the set, not of the order the merges
/// happened in. A parent is never above its child except where the parent is
/// a pinned root, which has no parent of its own, so [`Self::flatten`] still
/// finishes in one ascending pass and hands the parent and delta arrays out as
/// a map from every id to its representative and its offset from it.
#[derive(Clone)]
pub struct UnionFind {
    parent: Vec<u32>,
    delta: Vec<u64>,
    /// Per id, its carrier's [`mask`]. Read at the representative.
    mask: Vec<u64>,
    pinned: Vec<bool>,
    /// Per id, whether a union tried to put it at a non-zero offset from
    /// itself. Read at the representative.
    conflict: Vec<bool>,
    /// Whether some delta has been non-zero. Until one is, [`Self::flatten`]
    /// and the table repair it feeds skip the deltas.
    shifted: bool,
    /// The lowest id a union absorbed since the last [`Self::flatten`], or
    /// `u32::MAX`. An id below it points straight at a representative: its
    /// parent is below it too, or a zero, and neither was absorbed.
    flat_below: u32,
}

impl Default for UnionFind {
    fn default() -> Self {
        Self {
            parent: Vec::new(),
            delta: Vec::new(),
            mask: Vec::new(),
            pinned: Vec::new(),
            conflict: Vec::new(),
            shifted: false,
            flat_below: u32::MAX,
        }
    }
}

impl UnionFind {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget every id, keeping what the vectors allocated.
    pub fn clear(&mut self) {
        self.parent.clear();
        self.delta.clear();
        self.mask.clear();
        self.pinned.clear();
        self.conflict.clear();
        self.shifted = false;
        self.flat_below = u32::MAX;
    }

    /// Mint an id in a set of its own, its offsets reduced by `mask`.
    pub fn push(&mut self, mask: u64, pinned: bool) -> u32 {
        let id = self.parent.len() as u32;
        self.parent.push(id);
        self.delta.push(0);
        self.mask.push(mask);
        self.pinned.push(pinned);
        self.conflict.push(false);
        id
    }

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Grow to `len` ids, each new one in a set of its own with no carrier.
    pub fn extend_to(&mut self, len: usize) {
        let from = self.parent.len() as u32;
        self.parent.extend(from..len as u32);
        self.delta.resize(len, 0);
        self.mask.resize(len, 0);
        self.pinned.resize(len, false);
        self.conflict.resize(len, false);
    }

    /// The representative of `id`'s set.
    #[inline]
    pub fn root(&self, id: u32) -> u32 {
        let mut cur = id;
        while self.parent[cur as usize] != cur {
            cur = self.parent[cur as usize];
        }
        cur
    }

    /// `reference` as its representative plus an offset.
    #[inline]
    pub fn find(&self, reference: Ref) -> Ref {
        if !self.shifted && reference.offset == 0 {
            return self.root(reference.id).into();
        }
        let mut cur = reference.id;
        let mut offset = reference.offset;
        while self.parent[cur as usize] != cur {
            offset = offset.wrapping_add(self.delta[cur as usize]);
            cur = self.parent[cur as usize];
        }
        Ref {
            id: cur,
            offset: offset & self.mask[cur as usize],
        }
    }

    /// The mask offsets on `id`'s set are reduced by.
    pub fn mask(&self, id: u32) -> u64 {
        self.mask[self.root(id) as usize]
    }

    /// Whether `id`'s representative is a carrier's zero.
    pub fn is_zero(&self, id: u32) -> bool {
        self.pinned[self.root(id) as usize]
    }

    /// Whether `id` itself is a carrier's zero: what [`Self::is_zero`] reads
    /// at a representative.
    #[inline]
    pub fn pinned(&self, id: u32) -> bool {
        self.pinned[id as usize]
    }

    /// Whether a union tried to put `id`'s set at a non-zero offset from
    /// itself.
    pub fn is_conflicted(&self, id: u32) -> bool {
        self.conflict[self.root(id) as usize]
    }

    /// Record `a == b`. A contradiction changes nothing but the conflict
    /// flags of the sets involved.
    pub fn union(&mut self, a: Ref, b: Ref) -> Merge {
        let (a, b) = (self.find(a), self.find(b));
        if a.id == b.id {
            if a.offset == b.offset {
                return Merge::Same;
            }
            self.conflict[a.id as usize] = true;
            return Merge::Contradiction;
        }
        let (pa, pb) = (self.pinned[a.id as usize], self.pinned[b.id as usize]);
        if pa && pb {
            self.conflict[a.id as usize] = true;
            self.conflict[b.id as usize] = true;
            return Merge::Contradiction;
        }
        let (survivor, absorbed) = match pa || (!pb && a.id < b.id) {
            true => (a, b),
            false => (b, a),
        };
        let mask = self.mask[survivor.id as usize] | self.mask[absorbed.id as usize];
        // absorbed.id + absorbed.offset == survivor.id + survivor.offset
        let delta = survivor.offset.wrapping_sub(absorbed.offset) & mask;
        self.parent[absorbed.id as usize] = survivor.id;
        self.delta[absorbed.id as usize] = delta;
        self.mask[survivor.id as usize] = mask;
        self.conflict[survivor.id as usize] |= self.conflict[absorbed.id as usize];
        self.shifted |= delta != 0;
        self.flat_below = self.flat_below.min(absorbed.id);
        Merge::Merged {
            survivor: survivor.id,
            absorbed: absorbed.id,
        }
    }

    /// Point every id straight at its representative.
    pub fn flatten(&mut self) {
        let from =
            (std::mem::replace(&mut self.flat_below, u32::MAX) as usize).min(self.parent.len());
        // Only an id whose parent is no longer a root takes a step. Which
        // parents are roots does not change here, so one scan lists those
        // ids; going up the ids finishes a parent before its children.
        let mut steps = Vec::new();
        tir_adt::simd::select_moved(&self.parent[from..], &self.parent, &mut steps);
        for i in steps {
            let i = from + i as usize;
            let parent = self.parent[i] as usize;
            let root = self.parent[parent];
            if self.shifted {
                let delta = self.delta[i].wrapping_add(self.delta[parent]);
                self.delta[i] = delta & self.mask[root as usize];
            }
            self.parent[i] = root;
        }
    }

    /// Id -> parent. After [`Self::flatten`], id -> representative.
    pub fn parents(&self) -> &[u32] {
        &self.parent
    }

    /// Id -> offset from its parent, or `None` while every one is zero.
    pub fn deltas(&self) -> Option<&[u64]> {
        self.shifted.then_some(&self.delta[..])
    }

    /// Id -> its carrier's mask; meaningful at a representative.
    pub fn masks(&self) -> &[u64] {
        &self.mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn seeded(classes: u32, width: u8) -> UnionFind {
        let mut uf = UnionFind::new();
        for _ in 0..classes {
            uf.push(mask(width), false);
        }
        uf
    }

    fn at(class: u32, offset: u64) -> Ref {
        Ref { id: class, offset }
    }

    #[test]
    fn a_set_is_represented_by_its_smallest_member() {
        let mut uf = seeded(4, 0);
        uf.union(at(3, 0), at(1, 0));
        uf.union(at(2, 0), at(0, 0));
        uf.union(at(3, 0), at(2, 0));
        for i in 0..4 {
            assert_eq!(uf.root(i), 0);
        }
    }

    #[test]
    fn a_zero_survives_a_smaller_class() {
        let mut uf = seeded(2, 8);
        let zero = uf.push(mask(8), true);
        // 1 == zero + 5: the class is the constant five.
        uf.union(at(1, 0), at(zero, 5));
        uf.union(at(0, 0), at(1, 2));
        assert_eq!(uf.find(at(1, 0)), at(zero, 5));
        assert_eq!(uf.find(at(0, 0)), at(zero, 7));
    }

    #[test]
    fn a_class_at_two_offsets_from_itself_is_a_contradiction() {
        let mut uf = seeded(2, 8);
        assert!(matches!(uf.union(at(0, 0), at(1, 3)), Merge::Merged { .. }));
        assert_eq!(uf.union(at(1, 3), at(0, 0)), Merge::Same);
        assert!(!uf.is_conflicted(1));
        assert_eq!(uf.union(at(1, 0), at(0, 0)), Merge::Contradiction);
        // Nothing moved, and the set is marked.
        assert_eq!(uf.find(at(1, 0)), at(0, 253));
        assert!(uf.is_conflicted(1));
    }

    proptest! {
        /// Against a model that keeps every class's representative and offset
        /// explicitly and relabels a whole set on each merge, at a width
        /// narrow enough to wrap.
        #[test]
        fn deltas_agree_with_a_relabelling_model(
            steps in prop::collection::vec((0u32..12, any::<u8>(), 0u32..12, any::<u8>()), 0..48),
        ) {
            const CLASSES: u32 = 12;
            let mut uf = seeded(CLASSES, 8);
            let mut model: Vec<(u32, u8)> = (0..CLASSES).map(|class| (class, 0)).collect();
            for (a, oa, b, ob) in steps {
                let (ra, da) = model[a as usize];
                let (rb, db) = model[b as usize];
                let (va, vb) = (da.wrapping_add(oa), db.wrapping_add(ob));
                let expected = match (ra == rb, va == vb) {
                    (true, true) => Merge::Same,
                    (true, false) => Merge::Contradiction,
                    (false, _) => {
                        let (survivor, absorbed) = if ra < rb { (ra, rb) } else { (rb, ra) };
                        // absorbed + x == survivor + y, as seen from a and b
                        let shift = if survivor == ra { va.wrapping_sub(vb) } else { vb.wrapping_sub(va) };
                        for entry in &mut model {
                            if entry.0 == absorbed {
                                *entry = (survivor, entry.1.wrapping_add(shift));
                            }
                        }
                        Merge::Merged { survivor, absorbed }
                    }
                };
                prop_assert_eq!(uf.union(at(a, u64::from(oa)), at(b, u64::from(ob))), expected);
            }
            let found: Vec<Ref> = (0..CLASSES).map(|i| uf.find(at(i, 0))).collect();
            let want: Vec<Ref> = model.iter().map(|&(root, offset)| at(root, u64::from(offset))).collect();
            prop_assert_eq!(&found, &want);
            uf.flatten();
            let flat: Vec<Ref> = (0..CLASSES as usize)
                .map(|i| at(uf.parents()[i], uf.deltas().map_or(0, |deltas| deltas[i])))
                .collect();
            prop_assert_eq!(flat, want);
        }
    }
}
