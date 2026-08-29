//! Lattice columns: what a class is *known* to be, as opposed to what it holds.
//!
//! A column is keyed on canonical class and joined — by a union, and by a rule
//! raising a value onto one already there. Absence is the bottom of the lattice
//! and [`Fact::Conflict`] its top, so a class proven two different values reads
//! back as nothing known: "proven contradictory" does not entail "proven to be
//! five", and failing the read is the conservative answer a refuted hypothesis
//! needs.
//!
//! Values only ever rise, which is what lets semi-naive saturation trust a
//! column's delta the way it trusts a row's: a match a fact enabled cannot be
//! un-enabled by a later round.

use std::hash::Hash;

use crate::ClassId;
use crate::label::FxHashMap;

/// What a class is known to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact<V> {
    Known(V),
    /// Two different values were proven of one class.
    Conflict,
}

impl<V: Eq> Fact<V> {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Fact::Known(a), Fact::Known(b)) if a == b => Fact::Known(a),
            _ => Fact::Conflict,
        }
    }
}

/// One lattice column.
#[derive(Debug)]
pub struct Column<V> {
    fact: FxHashMap<ClassId, Entry<V>>,
    /// Value -> the classes that have held it. Entries go stale when a class
    /// rises to `Conflict` or a scope pops, so a read confirms against `fact`.
    by_value: FxHashMap<V, Vec<ClassId>>,
    /// Per open scope, the keys it wrote and what they held before, innermost
    /// last. A scope never keeps a fact past its pop.
    scopes: Vec<Vec<(ClassId, Option<Entry<V>>)>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry<V> {
    fact: Fact<V>,
    /// The epoch the entry last rose in, for [`Column::is_new`].
    stamp: u32,
}

impl<V> Default for Column<V> {
    fn default() -> Self {
        Self {
            fact: FxHashMap::default(),
            by_value: FxHashMap::default(),
            scopes: Vec::new(),
        }
    }
}

impl<V: Copy + Eq + Hash> Column<V> {
    /// Join `value` onto what `class` is known to be, stamping the entry with
    /// `epoch`. Reports whether the column moved.
    pub fn raise(&mut self, class: ClassId, value: V, epoch: u32) -> bool {
        self.write(class, Fact::Known(value), epoch)
    }

    fn write(&mut self, class: ClassId, fact: Fact<V>, epoch: u32) -> bool {
        let previous = self.fact.get(&class).copied();
        let joined = match previous {
            Some(entry) => entry.fact.join(fact),
            None => fact,
        };
        if previous.is_some_and(|entry| entry.fact == joined) {
            return false;
        }
        if let Some(frame) = self.scopes.last_mut() {
            frame.push((class, previous));
        }
        self.fact.insert(
            class,
            Entry {
                fact: joined,
                stamp: epoch,
            },
        );
        if let Fact::Known(value) = joined {
            self.by_value.entry(value).or_default().push(class);
        }
        true
    }

    /// What `class` is known to be, or `None` for unknown and for a conflict.
    pub fn get(&self, class: ClassId) -> Option<V> {
        match self.fact.get(&class)?.fact {
            Fact::Known(value) => Some(value),
            Fact::Conflict => None,
        }
    }

    /// Whether the entry for `class` rose in the epoch that just ended.
    pub fn is_new(&self, class: ClassId, epoch: u32) -> bool {
        self.fact
            .get(&class)
            .is_some_and(|entry| entry.stamp + 1 == epoch)
    }

    /// Whether `class` was proven two different values.
    pub fn is_conflicted(&self, class: ClassId) -> bool {
        matches!(
            self.fact.get(&class),
            Some(Entry {
                fact: Fact::Conflict,
                ..
            })
        )
    }

    /// The classes known to be `value`, in the order they first were.
    pub fn classes_with(&self, value: V) -> impl Iterator<Item = ClassId> + '_ {
        self.by_value
            .get(&value)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&class| self.get(class) == Some(value))
    }

    /// Move `absorbed`'s value onto `survivor`, joining with whatever is there.
    /// Reports whether the survivor's entry moved.
    pub fn merge(&mut self, absorbed: ClassId, survivor: ClassId, epoch: u32) -> bool {
        let Some(entry) = self.fact.get(&absorbed).copied() else {
            return false;
        };
        if let Some(frame) = self.scopes.last_mut() {
            frame.push((absorbed, Some(entry)));
        }
        self.fact.remove(&absorbed);
        self.write(survivor, entry.fact, epoch)
    }

    /// The classes the open scopes have written. Duplicated across frames, and
    /// not canonicalized: a caller closing over them does both.
    pub fn scoped_keys(&self) -> impl Iterator<Item = ClassId> + '_ {
        self.scopes.iter().flatten().map(|&(class, _)| class)
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    /// Undo everything the innermost scope wrote.
    pub fn pop_scope(&mut self) {
        for (class, previous) in self.scopes.pop().expect("open scope").into_iter().rev() {
            match previous {
                Some(entry) => self.fact.insert(class, entry),
                None => self.fact.remove(&class),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(id: u32) -> ClassId {
        ClassId(id)
    }

    #[test]
    fn a_second_value_conflicts_and_reads_as_unknown() {
        let mut column = Column::default();
        assert!(column.raise(class(0), 7u32, 1));
        assert_eq!(column.get(class(0)), Some(7));
        assert!(!column.raise(class(0), 7, 1));
        assert!(column.raise(class(0), 9, 2));
        assert_eq!(column.get(class(0)), None);
        assert!(column.is_conflicted(class(0)));
    }

    #[test]
    fn a_merge_joins_the_absorbed_value_onto_the_survivor() {
        let mut column = Column::default();
        column.raise(class(1), 7u32, 1);
        assert!(column.merge(class(1), class(0), 2));
        assert_eq!(column.get(class(0)), Some(7));
        assert_eq!(column.get(class(1)), None);
    }

    #[test]
    fn a_scope_raise_is_undone_by_the_pop() {
        let mut column = Column::default();
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
        let mut column = Column::default();
        column.raise(class(0), 1u32, 1);
        column.raise(class(1), 1, 1);
        column.raise(class(1), 2, 2);
        let holders: Vec<ClassId> = column.classes_with(1).collect();
        assert_eq!(holders, vec![class(0)]);
    }

    #[test]
    fn a_raise_is_new_for_exactly_one_epoch() {
        let mut column = Column::default();
        column.raise(class(0), 1u32, 4);
        assert!(column.is_new(class(0), 5));
        assert!(!column.is_new(class(0), 6));
    }
}
