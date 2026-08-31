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

/// How a column combines two values proven of one class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    /// Equal values agree, different ones conflict. The lattice a proof lives
    /// in: two constants for one class is a refutation.
    Agree,
    /// The first value proven wins. For a column whose values are a property of
    /// the class rather than a claim about it — an e-node's type, which
    /// congruence already forces its class to agree on.
    First,
}

impl<V: Eq> Fact<V> {
    pub fn join(self, other: Self, how: Join) -> Self {
        match (self, other, how) {
            (Fact::Known(a), _, Join::First) => Fact::Known(a),
            (Fact::Known(a), Fact::Known(b), Join::Agree) if a == b => Fact::Known(a),
            _ => Fact::Conflict,
        }
    }
}

/// One lattice column.
#[derive(Debug)]
pub struct Column<V> {
    how: Join,
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

impl<V> Column<V> {
    pub fn new(how: Join) -> Self {
        Self {
            how,
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

    /// Replace `class`'s entry with `fact`, no join. For a column whose values
    /// name classes: what is stored may be stale — a base that has since been
    /// absorbed, or one whose own derivation deepened — so only the caller that
    /// can read it back to what it means may combine two of them.
    pub fn put(&mut self, class: ClassId, fact: Fact<V>, epoch: u32) -> bool {
        self.set(class, fact, epoch)
    }

    /// Join `fact` onto what `class` is known to be.
    pub fn write(&mut self, class: ClassId, fact: Fact<V>, epoch: u32) -> bool {
        let joined = match self.fact.get(&class) {
            Some(entry) => entry.fact.join(fact, self.how),
            None => fact,
        };
        self.set(class, joined, epoch)
    }

    fn set(&mut self, class: ClassId, joined: Fact<V>, epoch: u32) -> bool {
        let previous = self.fact.get(&class).copied();
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
        match self.entry(class)? {
            Fact::Known(value) => Some(value),
            Fact::Conflict => None,
        }
    }

    /// The whole entry, telling unknown from conflicted.
    pub fn entry(&self, class: ClassId) -> Option<Fact<V>> {
        self.fact.get(&class).map(|entry| entry.fact)
    }

    /// Remove `class`'s entry, logging it for the scope to put back.
    pub fn detach(&mut self, class: ClassId) -> Option<Fact<V>> {
        let entry = self.fact.remove(&class)?;
        if let Some(frame) = self.scopes.last_mut() {
            frame.push((class, Some(entry)));
        }
        Some(entry.fact)
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
        match self.detach(absorbed) {
            Some(fact) => self.write(survivor, fact, epoch),
            None => false,
        }
    }

    /// The classes the open scopes have written. Duplicated across frames, and
    /// not canonicalized: a caller closing over them does both.
    pub fn scoped_keys(&self) -> impl Iterator<Item = ClassId> + '_ {
        self.scoped_keys_from(0)
    }

    /// The same, from the `depth`th open scope inward.
    pub fn scoped_keys_from(&self, depth: usize) -> impl Iterator<Item = ClassId> + '_ {
        self.scopes[depth..]
            .iter()
            .flatten()
            .map(|&(class, _)| class)
    }

    /// Whether an open scope wrote `class`'s entry — an assumption, as opposed
    /// to what the class states about itself.
    pub fn written_in_scope(&self, class: ClassId) -> bool {
        self.scoped_keys().any(|written| written == class)
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
        let holders: Vec<ClassId> = column.classes_with(1).collect();
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
