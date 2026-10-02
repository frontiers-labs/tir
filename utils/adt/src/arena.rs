use std::cell::{Cell, OnceCell};

/// An append-only arena that allocates through a shared reference.
///
/// Values never move and are dropped with the arena, so a reference handed out
/// by [`Arena::alloc`] stays valid across any number of later allocations.
pub struct Arena<T> {
    /// Level `i` holds `2^i` slots and is allocated on first use: growing adds
    /// a level and leaves the earlier ones where they are.
    levels: [OnceCell<Box<[OnceCell<T>]>>; usize::BITS as usize],
    len: Cell<usize>,
}

impl<T> Arena<T> {
    pub fn new() -> Self {
        Self {
            levels: [const { OnceCell::new() }; usize::BITS as usize],
            len: Cell::new(0),
        }
    }

    /// Stores `value` and returns a reference that lives as long as the arena.
    pub fn alloc(&self, value: T) -> &T {
        // Slots are numbered from 1, so level `i` covers `2^i..2^(i + 1)`.
        let slot = self.len.get() + 1;
        self.len.set(slot);
        let level = slot.ilog2();
        let slots = self.levels[level as usize]
            .get_or_init(|| (0..1usize << level).map(|_| OnceCell::new()).collect());
        slots[slot - (1 << level)].get_or_init(|| value)
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_survive_growth() {
        let arena = Arena::new();
        let refs: Vec<&String> = (0..1000).map(|i| arena.alloc(i.to_string())).collect();
        for (i, value) in refs.iter().enumerate() {
            assert_eq!(**value, i.to_string());
        }
    }
}
