//! Maps and sets indexed by IR identities, iterated in ascending id order.

use std::collections::BTreeMap;

use crate::{BlockId, OpId, RegionId, ValueId};

/// An id that names an IR entity.
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

/// An ordered map keyed by IR identity.
#[derive(Clone, Debug)]
pub struct IdMap<K, V> {
    rows: BTreeMap<u32, (K, V)>,
}

impl<K, V> Default for IdMap<K, V> {
    fn default() -> Self {
        IdMap {
            rows: BTreeMap::new(),
        }
    }
}

impl<K: RowId, V> IdMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.rows.get(&key.row()).map(|held| &held.1)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.rows.get_mut(&key.row()).map(|held| &mut held.1)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.rows.contains_key(&key.row())
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.rows.insert(key.row(), (key, value)).map(|held| held.1)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.rows.remove(&key.row()).map(|held| held.1)
    }

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        Entry {
            entry: self.rows.entry(key.row()),
            key,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.rows.values().map(|held| (&held.0, &held.1))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|held| held.0)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|held| held.1)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.rows.values_mut().map(|held| &mut held.1)
    }
}

/// A slot of an [`IdMap`], filled or not; see `BTreeMap::entry`.
pub struct Entry<'a, K, V> {
    entry: std::collections::btree_map::Entry<'a, u32, (K, V)>,
    key: K,
}

impl<'a, K: RowId, V> Entry<'a, K, V> {
    pub fn or_insert_with(self, make: impl FnOnce() -> V) -> &'a mut V {
        &mut self.entry.or_insert_with(|| (self.key, make())).1
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

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.rows.into_values())
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
