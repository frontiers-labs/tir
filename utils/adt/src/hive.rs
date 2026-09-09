use std::mem::MaybeUninit;

/// Target byte size of one chunk's slot array.
const CHUNK_BYTES: usize = 64 * 1024;

struct Chunk<T> {
    /// Slots ever handed out from the top of this chunk.
    bump: u32,
    occupied: Box<[u64]>,
    slots: Box<[MaybeUninit<T>]>,
}

impl<T> Chunk<T> {
    fn new(len: usize) -> Self {
        let mut slots = Vec::with_capacity(len);
        slots.resize_with(len, MaybeUninit::uninit);
        Self {
            bump: 0,
            occupied: vec![0u64; len.div_ceil(64)].into_boxed_slice(),
            slots: slots.into_boxed_slice(),
        }
    }

    fn is_occupied(&self, offset: u32) -> bool {
        let offset = offset as usize;
        self.occupied[offset / 64] >> (offset % 64) & 1 == 1
    }

    fn set_occupied(&mut self, offset: u32, live: bool) {
        let offset = offset as usize;
        let bit = 1u64 << (offset % 64);
        if live {
            self.occupied[offset / 64] |= bit;
        } else {
            self.occupied[offset / 64] &= !bit;
        }
    }
}

/// A chunked pool with stable `u32` handles.
///
/// Values never move, so a handle stays valid until it is removed. A removed
/// value's slot is *not* handed out again: a handle names one value for the
/// life of the pool, so a caller may hold one across any number of removals
/// and a batch of handles is always ascending in the order they were minted.
/// Removing therefore reclaims what the value owns, not the slot itself.
pub struct Hive<T> {
    /// Slots come off the top of the last chunk; earlier chunks are full.
    chunks: Vec<Chunk<T>>,
    len: u32,
}

impl<T> Hive<T> {
    /// `log2` of the slots per chunk, picked so a chunk is about 64 KiB.
    const K: u32 = {
        assert!(size_of::<T>() > 0, "a hive slot must have a size");
        let slots = CHUNK_BYTES / size_of::<T>();
        let mut k = 0;
        while 1usize << (k + 1) <= slots {
            k += 1;
        }
        if k < 4 { 4 } else { k }
    };
    const N: usize = 1 << Self::K;

    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }

    fn split(handle: u32) -> (usize, u32) {
        ((handle >> Self::K) as usize, handle & (Self::N as u32 - 1))
    }

    fn join(chunk: usize, offset: u32) -> u32 {
        (chunk as u32) << Self::K | offset
    }

    /// Stores `value` and returns the handle naming it.
    pub fn insert(&mut self, value: T) -> u32 {
        self.insert_with(|_| value)
    }

    /// [`Hive::insert`] for a value that needs to know its own handle.
    pub fn insert_with(&mut self, value: impl FnOnce(u32) -> T) -> u32 {
        if self
            .chunks
            .last()
            .is_none_or(|chunk| chunk.bump as usize == Self::N)
        {
            self.chunks.push(Chunk::new(Self::N));
        }
        let index = self.chunks.len() - 1;
        let chunk = &mut self.chunks[index];
        let offset = chunk.bump;
        chunk.bump += 1;
        let handle = Self::join(index, offset);
        chunk.slots[offset as usize] = MaybeUninit::new(value(handle));
        chunk.set_occupied(offset, true);
        self.len += 1;
        handle
    }

    /// Spends the next handle without storing anything: the slot is dead from
    /// the start, so the handles after it stay where they would have been.
    pub fn skip(&mut self) {
        if self
            .chunks
            .last()
            .is_none_or(|chunk| chunk.bump as usize == Self::N)
        {
            self.chunks.push(Chunk::new(Self::N));
        }
        let chunk = self.chunks.last_mut().expect("a chunk was just ensured");
        chunk.bump += 1;
    }

    /// Takes the value back out. The slot stays spent, so `handle` names
    /// nothing from here on.
    ///
    /// # Panics
    /// If `handle` does not name a live value.
    pub fn remove(&mut self, handle: u32) -> T {
        let (index, offset) = Self::split(handle);
        let chunk = &mut self.chunks[index];
        assert!(chunk.is_occupied(offset), "handle names a live value");
        chunk.set_occupied(offset, false);
        self.len -= 1;
        unsafe { chunk.slots[offset as usize].assume_init_read() }
    }

    pub fn get(&self, handle: u32) -> Option<&T> {
        let (index, offset) = Self::split(handle);
        let chunk = self.chunks.get(index)?;
        chunk
            .is_occupied(offset)
            .then(|| unsafe { chunk.slots[offset as usize].assume_init_ref() })
    }

    pub fn get_mut(&mut self, handle: u32) -> Option<&mut T> {
        let (index, offset) = Self::split(handle);
        let chunk = self.chunks.get_mut(index)?;
        if !chunk.is_occupied(offset) {
            return None;
        }
        Some(unsafe { chunk.slots[offset as usize].assume_init_mut() })
    }

    /// The handle the next insert will return. Handles are bump-allocated
    /// and never reused, so this is also how many slots were ever handed out.
    pub fn next_handle(&self) -> u32 {
        match self.chunks.last() {
            Some(chunk) => ((self.chunks.len() - 1) << Self::K) as u32 + chunk.bump,
            None => 0,
        }
    }

    /// Live handles in ascending order.
    pub fn handles(&self) -> impl Iterator<Item = u32> + '_ {
        self.chunks.iter().enumerate().flat_map(|(index, chunk)| {
            (0..chunk.bump)
                .filter(move |&offset| chunk.is_occupied(offset))
                .map(move |offset| Self::join(index, offset))
        })
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Chunks currently allocated, live or partly spent.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Slots the allocated chunks hold, live or spent.
    pub fn capacity(&self) -> usize {
        self.chunk_count() * Self::N
    }

    /// Bytes the chunks hold, whether or not their slots are live.
    pub fn bytes(&self) -> usize {
        self.chunk_count() * (Self::N * size_of::<T>() + Self::N / 8)
    }
}

impl<T> Default for Hive<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Hive<T> {
    fn drop(&mut self) {
        if !std::mem::needs_drop::<T>() {
            return;
        }
        for chunk in self.chunks.iter_mut() {
            for offset in 0..chunk.bump {
                if chunk.is_occupied(offset) {
                    unsafe { chunk.slots[offset as usize].assume_init_drop() };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    #[test]
    fn insert_get_remove() {
        let mut hive = Hive::new();
        let a = hive.insert(7u32);
        let b = hive.insert(9u32);
        assert_eq!(hive.get(a), Some(&7));
        assert_eq!(hive.get(b), Some(&9));
        assert_eq!(hive.len(), 2);
        assert_eq!(hive.remove(a), 7);
        assert_eq!(hive.get(a), None);
        assert_eq!(hive.len(), 1);
    }

    #[test]
    fn removed_slots_are_never_handed_out_again() {
        let mut hive = Hive::new();
        let handles: Vec<_> = (0..4).map(|i| hive.insert(i as u64)).collect();
        hive.remove(handles[1]);
        hive.remove(handles[3]);
        let later: Vec<_> = (0..4).map(|i| hive.insert(100 + i)).collect();
        assert!(later.iter().all(|handle| !handles.contains(handle)));
        assert!(later.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn an_emptied_chunk_is_kept() {
        let mut hive: Hive<u64> = Hive::new();
        let handles: Vec<_> = (0..Hive::<u64>::N * 3)
            .map(|i| hive.insert(i as u64))
            .collect();
        assert_eq!(hive.chunk_count(), 3);
        for handle in &handles[..Hive::<u64>::N] {
            hive.remove(*handle);
        }
        assert_eq!(hive.chunk_count(), 3);
        assert_eq!(hive.len(), Hive::<u64>::N * 2);
    }

    #[test]
    fn handles_are_ascending_and_live() {
        let mut hive = Hive::new();
        let handles: Vec<_> = (0..1000u32).map(|i| hive.insert(i)).collect();
        for handle in handles.iter().step_by(3) {
            hive.remove(*handle);
        }
        let listed: Vec<_> = hive.handles().collect();
        assert!(listed.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(listed.len(), hive.len());
        assert!(listed.iter().all(|h| hive.get(*h).is_some()));
    }

    #[test]
    fn dropped_values_are_released() {
        let mut hive = Hive::new();
        let kept = hive.insert(String::from("kept"));
        let gone = hive.insert(String::from("gone"));
        assert_eq!(hive.remove(gone), "gone");
        assert_eq!(hive.get(kept).map(String::as_str), Some("kept"));
    }

    proptest! {
        #[test]
        fn matches_a_hashmap_model(ops in prop::collection::vec((any::<bool>(), any::<u32>()), 0..500)) {
            let mut hive = Hive::new();
            let mut model: HashMap<u32, u32> = HashMap::new();
            let mut live: Vec<u32> = Vec::new();
            for (insert, value) in ops {
                if insert || live.is_empty() {
                    let handle = hive.insert(value);
                    prop_assert!(model.insert(handle, value).is_none());
                    live.push(handle);
                } else {
                    let handle = live.swap_remove(value as usize % live.len());
                    prop_assert_eq!(hive.remove(handle), model.remove(&handle).unwrap());
                }
                prop_assert_eq!(hive.len(), model.len());
            }
            let mut listed: Vec<_> = hive.handles().collect();
            listed.sort_unstable();
            let mut expected: Vec<_> = model.keys().copied().collect();
            expected.sort_unstable();
            prop_assert_eq!(listed, expected);
            for (handle, value) in &model {
                prop_assert_eq!(hive.get(*handle), Some(value));
            }
        }
    }
}
