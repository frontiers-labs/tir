use std::{
    borrow::Borrow,
    collections::{HashMap, hash_map},
    hash::Hash,
    iter::Zip,
    ops::Index,
    slice, vec,
};

use crate::FxBuildHasher;

/// A hash map that iterates in insertion order. Entries cannot be removed, so
/// that order never changes.
#[derive(Clone, PartialEq)]
pub struct IndexMap<K: Eq + Hash, V> {
    indices: HashMap<K, usize, FxBuildHasher>,
    keys: Vec<K>,
    values: Vec<V>,
}

impl<K: Eq + Hash, V> Default for IndexMap<K, V> {
    fn default() -> Self {
        Self {
            indices: HashMap::default(),
            keys: Vec::new(),
            values: Vec::new(),
        }
    }
}

impl<K: Eq + Hash, V> IndexMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.indices.get(k).map(|idx| &self.values[*idx])
    }

    pub fn contains_key<Q>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.indices.contains_key(k)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn keys(&self) -> slice::Iter<'_, K> {
        self.keys.iter()
    }

    pub fn iter(&self) -> Zip<slice::Iter<'_, K>, slice::Iter<'_, V>> {
        self.keys.iter().zip(&self.values)
    }
}

impl<K: Clone + Eq + Hash, V> IndexMap<K, V> {
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        match self.indices.entry(k) {
            hash_map::Entry::Occupied(entry) => {
                Some(std::mem::replace(&mut self.values[*entry.get()], v))
            }
            hash_map::Entry::Vacant(entry) => {
                self.keys.push(entry.key().clone());
                entry.insert(self.values.len());
                self.values.push(v);
                None
            }
        }
    }

    pub fn entry(&mut self, k: K) -> Entry<'_, K, V> {
        Entry { map: self, key: k }
    }
}

impl<K: Eq + Hash + Borrow<Q>, Q: Eq + Hash + ?Sized, V> Index<&Q> for IndexMap<K, V> {
    type Output = V;

    fn index(&self, k: &Q) -> &V {
        self.get(k).expect("no entry found for key")
    }
}

pub struct Entry<'a, K: Eq + Hash, V> {
    map: &'a mut IndexMap<K, V>,
    key: K,
}

impl<'a, K: Clone + Eq + Hash, V> Entry<'a, K, V> {
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        let IndexMap {
            indices,
            keys,
            values,
        } = self.map;
        let idx = *indices.entry(self.key).or_insert_with_key(|key| {
            keys.push(key.clone());
            values.push(V::default());
            values.len() - 1
        });
        &mut values[idx]
    }
}

impl<K: Clone + Eq + Hash, V> FromIterator<(K, V)> for IndexMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut map = Self::new();
        for (key, value) in iter {
            map.insert(key, value);
        }
        map
    }
}

impl<K: Eq + Hash, V> IntoIterator for IndexMap<K, V> {
    type Item = (K, V);
    type IntoIter = Zip<vec::IntoIter<K>, vec::IntoIter<V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.keys.into_iter().zip(self.values)
    }
}

impl<'a, K: Eq + Hash, V> IntoIterator for &'a IndexMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Zip<slice::Iter<'a, K>, slice::Iter<'a, V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::IndexMap;

    #[test]
    fn reinserting_a_key_replaces_its_value_in_place() {
        let mut map = IndexMap::new();
        map.insert("z", 1);
        map.insert("a", 2);

        assert_eq!(map.insert("z", 3), Some(1));
        assert_eq!(map.into_iter().collect::<Vec<_>>(), [("z", 3), ("a", 2)]);
    }
}
