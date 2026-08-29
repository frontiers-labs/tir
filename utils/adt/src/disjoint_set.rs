use std::cell::UnsafeCell;

// Partial path compression: compress the first N nodes (avoids recursion stack
// overflow on deep chains and the re-alloc cost of an unbounded buffer).
const AUTOCOMPRESS_DEPTH: usize = 16;

struct DisjointSetImpl {
    parents: UnsafeCell<Vec<i32>>,
}

impl DisjointSetImpl {
    fn with_size(size: usize) -> Self {
        Self {
            parents: UnsafeCell::new(vec![-1; size]),
        }
    }

    fn new() -> Self {
        Self {
            parents: UnsafeCell::new(Vec::new()),
        }
    }

    fn len(&self) -> usize {
        unsafe { &*self.parents.get() }.len()
    }

    fn push(&mut self) -> u32 {
        let p = unsafe { &mut *self.parents.get() };
        let id = p.len() as u32;
        p.push(-1);
        id
    }

    fn find_root(&self, i: u32) -> u32 {
        assert!(i <= i32::MAX as u32);

        let p = unsafe { &mut *self.parents.get() };

        let mut curr = i as usize;

        if p[i as usize] >= 0 {
            let mut backprop = [0; AUTOCOMPRESS_DEPTH];
            let mut ptr = 0;

            while p[curr] >= 0 {
                if ptr < backprop.len() {
                    backprop[ptr] = curr;
                    ptr += 1;
                }
                curr = p[curr] as usize;
            }

            for x in 0..ptr {
                p[backprop[x]] = curr as i32;
            }

            p[i as usize] = curr as i32;
        }

        curr as u32
    }

    fn connected(&self, i: u32, j: u32) -> bool {
        self.find_root(i) == self.find_root(j)
    }

    fn union(&mut self, i: u32, j: u32) -> (u32, u32, bool) {
        let mut root_i = self.find_root(i);
        let mut root_j = self.find_root(j);

        if root_i == root_j {
            return (root_i, root_j, false);
        }

        let p = unsafe { &mut *self.parents.get() };
        let mut i_size = -p[root_i as usize];
        let mut j_size = -p[root_j as usize];

        if i_size > j_size {
            std::mem::swap(&mut root_i, &mut root_j);
            std::mem::swap(&mut i_size, &mut j_size);
        }

        p[root_j as usize] -= i_size;
        p[root_i as usize] = root_j as i32;

        (root_i, root_j, true)
    }

    fn is_root(&self, i: u32) -> bool {
        let v = unsafe { (&*self.parents.get())[i as usize] };
        v < 0
    }
}

pub struct DisjointSet {
    uf: DisjointSetImpl,
}

impl Default for DisjointSet {
    fn default() -> Self {
        Self::empty()
    }
}

impl DisjointSet {
    pub fn new(size: usize) -> Self {
        Self {
            uf: DisjointSetImpl::with_size(size),
        }
    }

    /// An empty set that grows one element at a time via [`Self::push`].
    pub fn empty() -> Self {
        Self {
            uf: DisjointSetImpl::new(),
        }
    }

    /// Add a fresh singleton element, returning its id.
    pub fn push(&mut self) -> u32 {
        self.uf.push()
    }

    pub fn len(&self) -> usize {
        self.uf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.uf.len() == 0
    }

    pub fn find_root(&self, i: u32) -> u32 {
        self.uf.find_root(i)
    }

    pub fn union(&mut self, i: u32, j: u32) -> u32 {
        self.uf.union(i, j).1
    }

    pub fn connected(&self, i: u32, j: u32) -> bool {
        self.uf.connected(i, j)
    }
}

pub struct DisjointMap<V, F> {
    uf: DisjointSetImpl,
    values: Vec<Option<V>>,
    merge: F,
}

impl<V, F> DisjointMap<V, F>
where
    F: Fn(V, V) -> V,
{
    pub fn new(merge: F) -> Self {
        Self {
            uf: DisjointSetImpl::new(),
            values: Vec::new(),
            merge,
        }
    }

    pub fn len(&self) -> usize {
        self.uf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.uf.len() == 0
    }

    pub fn push(&mut self, value: V) -> u32 {
        let id = self.uf.push();
        self.values.push(Some(value));
        id
    }

    pub fn find_root(&self, i: u32) -> u32 {
        self.uf.find_root(i)
    }

    pub fn get(&self, i: u32) -> &V {
        self.values[self.find_root(i) as usize]
            .as_ref()
            .expect("root value")
    }

    pub fn set(&mut self, i: u32, value: V) {
        let root = self.find_root(i);
        self.values[root as usize] = Some(value);
    }

    pub fn union(&mut self, i: u32, j: u32) -> u32 {
        let (root_i, root_j, merged) = self.uf.union(i, j);
        if merged {
            let val_i = self.values[root_i as usize].take().expect("root value");
            let val_j = self.values[root_j as usize].take().expect("root value");
            self.values[root_j as usize] = Some((self.merge)(val_j, val_i));
            self.values[root_i as usize] = None;
        }
        root_j
    }

    pub fn connected(&self, i: u32, j: u32) -> bool {
        self.uf.connected(i, j)
    }

    pub fn roots(&self) -> impl Iterator<Item = (u32, &V)> {
        self.values.iter().enumerate().filter_map(|(i, value)| {
            if self.uf.is_root(i as u32) {
                Some((i as u32, value.as_ref().expect("root value")))
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_elements_are_disjoint() {
        let ds = DisjointSet::new(4);
        for i in 0..4 {
            assert_eq!(ds.find_root(i), i);
        }
        for i in 0..4 {
            for j in (i + 1)..4 {
                assert!(!ds.connected(i, j));
            }
        }
    }

    #[test]
    fn union_merges_two_sets() {
        let mut ds = DisjointSet::new(4);
        let root = ds.union(0, 2);
        assert!(ds.connected(0, 2));
        assert!(!ds.connected(0, 1));
        assert_eq!(ds.find_root(0), root);
        assert_eq!(ds.find_root(2), root);
    }

    #[test]
    fn union_is_transitive() {
        let mut ds = DisjointSet::new(5);
        ds.union(0, 1);
        ds.union(1, 2);
        assert!(ds.connected(0, 2));
        assert!(!ds.connected(0, 3));
    }

    #[test]
    fn union_same_element_is_noop() {
        let mut ds = DisjointSet::new(3);
        assert_eq!(ds.union(1, 1), 1);
        assert_eq!(ds.find_root(1), 1);
        assert!(!ds.connected(0, 1));
    }

    #[test]
    fn repeated_union_is_idempotent() {
        let mut ds = DisjointSet::new(3);
        let first = ds.union(0, 1);
        let second = ds.union(0, 1);
        assert_eq!(first, second);
        assert!(ds.connected(0, 1));
    }

    #[test]
    fn union_links_through_parent_chain() {
        let mut ds = DisjointSet::new(4);
        ds.union(0, 1);
        ds.union(2, 3);
        let root = ds.union(1, 2);
        for i in 0..4 {
            assert_eq!(ds.find_root(i), root);
        }
    }

    #[test]
    fn map_push_and_get() {
        let mut map = DisjointMap::new(|a: i32, b| a + b);
        let a = map.push(1);
        let b = map.push(14);
        assert_eq!(map.get(a), &1);
        assert_eq!(map.get(b), &14);
    }

    #[test]
    fn map_union_merges_values() {
        let mut map = DisjointMap::new(|a: i32, b| a + b);
        let a = map.push(1);
        let b = map.push(14);
        map.union(a, b);
        assert_eq!(map.get(a), &15);
        assert_eq!(map.get(b), &15);
    }

    #[test]
    fn map_union_merges_transitively() {
        let mut map = DisjointMap::new(|a: i32, b| a + b);
        let a = map.push(1);
        let b = map.push(14);
        let c = map.push(4);
        map.union(a, b);
        map.union(a, c);
        assert_eq!(map.get(c), &19);
        assert_eq!(map.find_root(c), map.find_root(a));
    }

    #[test]
    fn map_set_updates_class() {
        let mut map = DisjointMap::new(|a: i32, b| a + b);
        let a = map.push(1);
        let b = map.push(14);
        let c = map.push(4);
        map.union(a, b);
        map.union(a, c);
        map.set(c, 42);
        assert_eq!(map.get(b), &42);
    }
}
