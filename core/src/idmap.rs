//! Columns a pass derives over the rows of one function: a value per
//! operation, value, block or region id, stored by row position.
//!
//! An id is a chunk index and a row inside the chunk, and a function's rows
//! sit in a handful of chunks. An [`IdMap`] keeps one vector per chunk it has
//! seen, indexed by the row, so a lookup is a scan of those few chunk indices
//! and one load, with no hashing. Iteration is in ascending id order.

use tir_relational::chunk::{chunk_of, row_of};

use crate::{BlockId, OpId, RegionId, ValueId};

/// An id that names a row of the IR's storage.
pub trait RowId: Copy {
    fn row(self) -> u32;
}

impl RowId for OpId {
    fn row(self) -> u32 {
        self.number()
    }
}

impl RowId for ValueId {
    fn row(self) -> u32 {
        self.number()
    }
}

impl RowId for BlockId {
    fn row(self) -> u32 {
        self.number()
    }
}

impl RowId for RegionId {
    fn row(self) -> u32 {
        self.number()
    }
}

/// The entries of one chunk, a slot per row up to the highest written.
type Slots<K, V> = Vec<Option<(K, V)>>;

/// `id -> value` over the rows of a few chunks, with the interface of a
/// `HashMap`.
#[derive(Clone, Debug)]
pub struct IdMap<K, V> {
    /// The chunks seen, in the order they were first written.
    chunks: Vec<(u32, Slots<K, V>)>,
    len: usize,
}

impl<K, V> Default for IdMap<K, V> {
    fn default() -> Self {
        IdMap {
            chunks: Vec::new(),
            len: 0,
        }
    }
}

impl<K: RowId, V> IdMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn slot(&self, key: K) -> Option<&(K, V)> {
        let (chunk, row) = (chunk_of(key.row()), row_of(key.row()));
        let rows = &self.chunks.iter().find(|held| held.0 == chunk)?.1;
        rows.get(row)?.as_ref()
    }

    fn slot_mut(&mut self, key: K) -> &mut Option<(K, V)> {
        let (chunk, row) = (chunk_of(key.row()), row_of(key.row()));
        let at = match self.chunks.iter().position(|held| held.0 == chunk) {
            Some(at) => at,
            None => {
                self.chunks.push((chunk, Vec::new()));
                self.chunks.len() - 1
            }
        };
        let rows = &mut self.chunks[at].1;
        if rows.len() <= row {
            rows.resize_with(row + 1, || None);
        }
        &mut rows[row]
    }

    #[inline]
    pub fn get(&self, key: &K) -> Option<&V> {
        self.slot(*key).map(|held| &held.1)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let (chunk, row) = (chunk_of(key.row()), row_of(key.row()));
        let rows = &mut self.chunks.iter_mut().find(|held| held.0 == chunk)?.1;
        rows.get_mut(row)?.as_mut().map(|held| &mut held.1)
    }

    #[inline]
    pub fn contains_key(&self, key: &K) -> bool {
        self.slot(*key).is_some()
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let old = self.slot_mut(key).replace((key, value));
        self.len += usize::from(old.is_none());
        old.map(|held| held.1)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (chunk, row) = (chunk_of(key.row()), row_of(key.row()));
        let rows = &mut self.chunks.iter_mut().find(|held| held.0 == chunk)?.1;
        let old = rows.get_mut(row)?.take()?;
        self.len -= 1;
        Some(old.1)
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
        self.len = 0;
    }

    pub fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        Entry { map: self, key }
    }

    fn ordered(&self) -> impl Iterator<Item = &Slots<K, V>> {
        let mut order: Vec<usize> = (0..self.chunks.len()).collect();
        order.sort_unstable_by_key(|at| self.chunks[*at].0);
        order.into_iter().map(|at| &self.chunks[at].1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.ordered()
            .flat_map(|rows| rows.iter().flatten())
            .map(|held| (&held.0, &held.1))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|held| held.0)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|held| held.1)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.chunks.sort_unstable_by_key(|held| held.0);
        self.chunks
            .iter_mut()
            .flat_map(|held| held.1.iter_mut().flatten())
            .map(|held| &mut held.1)
    }
}

/// A slot of an [`IdMap`], filled or not; see `HashMap::entry`.
pub struct Entry<'a, K, V> {
    map: &'a mut IdMap<K, V>,
    key: K,
}

impl<'a, K: RowId, V> Entry<'a, K, V> {
    pub fn or_insert_with(self, make: impl FnOnce() -> V) -> &'a mut V {
        let key = self.key;
        if !self.map.contains_key(&key) {
            self.map.len += 1;
        }
        &mut self
            .map
            .slot_mut(key)
            .get_or_insert_with(|| (key, make()))
            .1
    }

    pub fn or_insert(self, value: V) -> &'a mut V {
        self.or_insert_with(|| value)
    }

    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }
}

impl<K: RowId, V> std::ops::Index<&K> for IdMap<K, V> {
    type Output = V;

    fn index(&self, key: &K) -> &V {
        self.get(key).expect("an id the map holds")
    }
}

impl<K: RowId, V> Extend<(K, V)> for IdMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, pairs: I) {
        for (key, value) in pairs {
            self.insert(key, value);
        }
    }
}

impl<K: RowId, V> FromIterator<(K, V)> for IdMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(pairs: I) -> Self {
        let mut map = IdMap::new();
        map.extend(pairs);
        map
    }
}

impl<'a, K: RowId, V> IntoIterator for &'a IdMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Box<dyn Iterator<Item = (&'a K, &'a V)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<K: RowId + 'static, V: 'static> IntoIterator for IdMap<K, V> {
    type Item = (K, V);
    type IntoIter = Box<dyn Iterator<Item = (K, V)>>;

    fn into_iter(mut self) -> Self::IntoIter {
        self.chunks.sort_unstable_by_key(|held| held.0);
        Box::new(
            self.chunks
                .into_iter()
                .flat_map(|held| held.1.into_iter().flatten()),
        )
    }
}

/// A set of ids, with the interface of a `HashSet`.
#[derive(Clone, Debug)]
pub struct IdSet<K>(IdMap<K, ()>);

impl<K> Default for IdSet<K> {
    fn default() -> Self {
        IdSet(IdMap::default())
    }
}

impl<K: RowId> IdSet<K> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Add `key`, reporting whether it was absent.
    pub fn insert(&mut self, key: K) -> bool {
        self.0.insert(key, ()).is_none()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    #[inline]
    pub fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }

    pub fn remove(&mut self, key: &K) -> bool {
        self.0.remove(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = &K> {
        self.0.keys()
    }
}

impl<K: RowId> Extend<K> for IdSet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, keys: I) {
        for key in keys {
            self.insert(key);
        }
    }
}

impl<K: RowId> FromIterator<K> for IdSet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(keys: I) -> Self {
        let mut set = IdSet::new();
        set.extend(keys);
        set
    }
}

impl<'a, K: RowId> IntoIterator for &'a IdSet<K> {
    type Item = &'a K;
    type IntoIter = Box<dyn Iterator<Item = &'a K> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}
