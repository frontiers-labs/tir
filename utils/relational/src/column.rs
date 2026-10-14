//! Lattice columns: what an id is *known* to be, as opposed to what the tables
//! hold about it.
//!
//! A column is keyed on a canonical id and joined — when two ids are equated,
//! and when a rule raises a value onto one already there. Absence is the bottom
//! of the lattice and [`Fact::Conflict`] its top, so an id proven two different
//! values reads back as nothing known: "proven contradictory" does not entail
//! "proven to be five", and failing the read is the conservative answer a
//! refuted hypothesis needs.
//!
//! Values only ever rise, which is what lets semi-naive saturation trust a
//! column's delta the way it trusts a row's: a match a fact enabled cannot be
//! un-enabled by a later round.

use std::hash::Hash;

use crate::FxHashMap;

/// What an id is known to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact<V> {
    Known(V),
    /// Two different values were proven of one id.
    Conflict,
}

/// How a column combines two values proven of one id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    /// Equal values agree, different ones conflict. The lattice a proof lives
    /// in: two constants for one id is a refutation.
    Agree,
    /// The first value proven wins. For a column whose values are a property of
    /// the id rather than a claim about it, which the client already forces
    /// every equated id to agree on.
    First,
    /// The greater value wins: a bound that only rises, as a rule's head
    /// raises it. A guard that compares it holds of the value a round read,
    /// so the rules that raise such a column are saturated before any rule
    /// that compares it.
    Max,
    /// The lesser value wins; see [`Join::Max`].
    Min,
    /// The values add up: a count of what a client says once each of the
    /// ids that end up one. Saying a value twice counts it twice, so no
    /// rule raises such a column.
    Sum,
}

impl<V: Ord + std::ops::Add<Output = V>> Fact<V> {
    pub fn join(self, other: Self, how: Join) -> Self {
        match (self, other, how) {
            (Fact::Known(a), _, Join::First) => Fact::Known(a),
            (Fact::Known(a), Fact::Known(b), Join::Agree) if a == b => Fact::Known(a),
            (Fact::Known(a), Fact::Known(b), Join::Max) => Fact::Known(a.max(b)),
            (Fact::Known(a), Fact::Known(b), Join::Min) => Fact::Known(a.min(b)),
            (Fact::Known(a), Fact::Known(b), Join::Sum) => Fact::Known(a + b),
            _ => Fact::Conflict,
        }
    }
}

/// One lattice column.
#[derive(Debug)]
pub struct Column<V> {
    how: Join,
    fact: FxHashMap<u32, Entry<V>>,
    /// Value -> the ids that have held it. Entries go stale when an id
    /// rises to `Conflict` or a scope pops, so a read confirms against `fact`.
    by_value: FxHashMap<V, Vec<u32>>,
    /// Per open scope, the keys it wrote and what they held before, innermost
    /// last. A scope never keeps a fact past its pop.
    scopes: Vec<Vec<(u32, Option<Entry<V>>)>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry<V> {
    fact: Fact<V>,
    /// The epoch the entry last rose in, for [`Column::is_new`].
    stamp: u32,
}

impl<V> Column<V> {
    pub fn new(how: Join) -> Self {
        Self {
            how,
            fact: FxHashMap::default(),
            by_value: FxHashMap::default(),
            scopes: Vec::new(),
        }
    }

    /// Forget every key, as a column just made.
    pub fn clear(&mut self) {
        *self = Self::new(self.how);
    }
}

impl<V: Copy + Ord + Hash + std::ops::Add<Output = V>> Column<V> {
    /// Join `value` onto what `key` is known to be, stamping the entry with
    /// `epoch`. Reports whether the column moved.
    pub fn raise(&mut self, key: u32, value: V, epoch: u32) -> bool {
        self.write(key, Fact::Known(value), epoch)
    }

    /// Join `fact` onto what `key` is known to be.
    pub fn write(&mut self, key: u32, fact: Fact<V>, epoch: u32) -> bool {
        let joined = match self.fact.get(&key) {
            Some(entry) => entry.fact.join(fact, self.how),
            None => fact,
        };
        self.set(key, joined, epoch)
    }

    fn set(&mut self, key: u32, joined: Fact<V>, epoch: u32) -> bool {
        let previous = self.fact.get(&key).copied();
        if previous.is_some_and(|entry| entry.fact == joined) {
            return false;
        }
        if let Some(frame) = self.scopes.last_mut() {
            frame.push((key, previous));
        }
        self.fact.insert(
            key,
            Entry {
                fact: joined,
                stamp: epoch,
            },
        );
        if let Fact::Known(value) = joined {
            self.by_value.entry(value).or_default().push(key);
        }
        true
    }

    /// What `key` is known to be, or `None` for unknown and for a conflict.
    pub fn get(&self, key: u32) -> Option<V> {
        match self.entry(key)? {
            Fact::Known(value) => Some(value),
            Fact::Conflict => None,
        }
    }

    /// The whole entry, telling unknown from conflicted.
    pub fn entry(&self, key: u32) -> Option<Fact<V>> {
        self.fact.get(&key).map(|entry| entry.fact)
    }

    /// Remove `key`'s entry, logging it for the scope to put back.
    pub fn detach(&mut self, key: u32) -> Option<Fact<V>> {
        let entry = self.fact.remove(&key)?;
        if let Some(frame) = self.scopes.last_mut() {
            frame.push((key, Some(entry)));
        }
        Some(entry.fact)
    }

    /// Whether the entry for `key` rose in the epoch that just ended.
    pub fn is_new(&self, key: u32, epoch: u32) -> bool {
        self.fact
            .get(&key)
            .is_some_and(|entry| entry.stamp + 1 == epoch)
    }

    /// Whether `key` was proven two different values.
    pub fn is_conflicted(&self, key: u32) -> bool {
        matches!(
            self.fact.get(&key),
            Some(Entry {
                fact: Fact::Conflict,
                ..
            })
        )
    }

    /// The ids known to be `value`, in the order they first were.
    pub fn keys_with(&self, value: V) -> impl Iterator<Item = u32> + '_ {
        self.by_value
            .get(&value)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&key| self.get(key) == Some(value))
    }

    /// Move `absorbed`'s value onto `survivor`, joining with whatever is there.
    /// Reports whether the survivor's entry moved.
    pub fn merge(&mut self, absorbed: u32, survivor: u32, epoch: u32) -> bool {
        if self.fact.is_empty() {
            return false;
        }
        match self.detach(absorbed) {
            Some(fact) => self.write(survivor, fact, epoch),
            None => false,
        }
    }

    /// Whether no id has an entry.
    pub fn is_empty(&self) -> bool {
        self.fact.is_empty()
    }

    /// The ids the open scopes have written. Duplicated across frames, and
    /// not canonicalized: a caller closing over them does both.
    pub fn scoped_keys(&self) -> impl Iterator<Item = u32> + '_ {
        self.scoped_keys_from(0)
    }

    /// The same, from the `depth`th open scope inward.
    pub fn scoped_keys_from(&self, depth: usize) -> impl Iterator<Item = u32> + '_ {
        self.scopes[depth..].iter().flatten().map(|&(key, _)| key)
    }

    /// Whether an open scope wrote `key`'s entry — an assumption, as opposed
    /// to what the id was known to be before the scope opened.
    pub fn written_in_scope(&self, key: u32) -> bool {
        self.scoped_keys().any(|written| written == key)
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    /// Undo everything the innermost scope wrote.
    pub fn pop_scope(&mut self) {
        for (key, previous) in self.scopes.pop().expect("open scope").into_iter().rev() {
            match previous {
                Some(entry) => self.fact.insert(key, entry),
                None => self.fact.remove(&key),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(id: u32) -> u32 {
        id
    }

    fn agreeing<V>() -> Column<V> {
        Column::new(Join::Agree)
    }

    #[test]
    fn a_first_wins_column_keeps_what_it_was_told_first() {
        let mut column = Column::new(Join::First);
        column.raise(class(0), 7u32, 1);
        column.raise(class(0), 9, 2);
        assert_eq!(column.get(class(0)), Some(7));
    }

    #[test]
    fn a_second_value_conflicts_and_reads_as_unknown() {
        let mut column = agreeing();
        assert!(column.raise(class(0), 7u32, 1));
        assert_eq!(column.get(class(0)), Some(7));
        assert!(!column.raise(class(0), 7, 1));
        assert!(column.raise(class(0), 9, 2));
        assert_eq!(column.get(class(0)), None);
        assert!(column.is_conflicted(class(0)));
    }

    #[test]
    fn a_merge_joins_the_absorbed_value_onto_the_survivor() {
        let mut column = agreeing();
        column.raise(class(1), 7u32, 1);
        assert!(column.merge(class(1), class(0), 2));
        assert_eq!(column.get(class(0)), Some(7));
        assert_eq!(column.get(class(1)), None);
    }

    #[test]
    fn a_scope_raise_is_undone_by_the_pop() {
        let mut column = agreeing();
        column.raise(class(0), 1u32, 1);
        column.push_scope();
        column.raise(class(0), 0, 2);
        column.raise(class(1), 5, 2);
        assert!(column.is_conflicted(class(0)));
        assert_eq!(column.get(class(1)), Some(5));
        column.pop_scope();
        assert_eq!(column.get(class(0)), Some(1));
        assert_eq!(column.get(class(1)), None);
    }

    #[test]
    fn the_reverse_index_skips_classes_that_moved_on() {
        let mut column = agreeing();
        column.raise(class(0), 1u32, 1);
        column.raise(class(1), 1, 1);
        column.raise(class(1), 2, 2);
        let holders: Vec<u32> = column.keys_with(1).collect();
        assert_eq!(holders, vec![class(0)]);
    }

    #[test]
    fn a_raise_is_new_for_exactly_one_epoch() {
        let mut column = agreeing();
        column.raise(class(0), 1u32, 4);
        assert!(column.is_new(class(0), 5));
        assert!(!column.is_new(class(0), 6));
    }
}
