use std::{collections::VecDeque, mem::ManuallyDrop, mem::MaybeUninit};

/// Sentinel handle: no slot.
const NONE: u32 = u32::MAX;

/// Target byte size of one chunk's slot array.
const CHUNK_BYTES: usize = 64 * 1024;

union Slot<T> {
    value: ManuallyDrop<T>,
    next_free: u32,
}

struct Chunk<T> {
    live: u32,
    /// Head of this chunk's LIFO free list, an offset within the chunk.
    free_head: u32,
    /// Slots ever handed out from the top of this chunk.
    bump: u32,
    occupied: Box<[u64]>,
    slots: Box<[MaybeUninit<Slot<T>>]>,
}

impl<T> Chunk<T> {
    fn new(len: usize) -> Self {
        let mut slots = Vec::with_capacity(len);
        slots.resize_with(len, MaybeUninit::uninit);
        Self {
            live: 0,
            free_head: NONE,
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
/// Values never move, so a handle stays valid until it is removed. Slots and
/// whole chunks are reclaimed on removal, which means handles are *reused*:
/// a handle of a removed value can name a later insertion.
pub struct Hive<T> {
    /// Chunk index is stable; a freed chunk leaves a `None` hole to be refilled.
    chunks: Vec<Option<Chunk<T>>>,
    /// Chunks with a non-empty free list, lowest index first.
    partial: VecDeque<u32>,
    /// Chunk currently handing out slots from the top, or [`NONE`].
    open: u32,
    /// Slots whose value has been removed but which are not yet on their
    /// chunk's free list. Reuse is published by [`Hive::recycle`] so a caller
    /// holding a handle across a removal cannot have it answer for a stranger
    /// until it says it is done with the old ones.
    pending: Vec<u32>,
    len: u32,
}

impl<T> Hive<T> {
    /// `log2` of the slots per chunk, picked so a chunk is about 64 KiB.
    const K: u32 = {
        assert!(
            size_of::<T>() >= 4,
            "a hive slot must hold a free-list link"
        );
        let slots = CHUNK_BYTES / size_of::<Slot<T>>();
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
            partial: VecDeque::new(),
            open: NONE,
            pending: Vec::new(),
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
        let (index, offset) = self.free_slot();
        let value = value(Self::join(index, offset));
        let chunk = self.chunks[index]
            .as_mut()
            .expect("free slot in live chunk");
        chunk.slots[offset as usize] = MaybeUninit::new(Slot {
            value: ManuallyDrop::new(value),
        });
        chunk.set_occupied(offset, true);
        chunk.live += 1;
        self.len += 1;
        Self::join(index, offset)
    }

    fn free_slot(&mut self) -> (usize, u32) {
        if let Some(&index) = self.partial.front() {
            let index = index as usize;
            let chunk = self.chunks[index].as_mut().expect("partial chunk is live");
            let offset = chunk.free_head;
            chunk.free_head = unsafe { chunk.slots[offset as usize].assume_init_ref().next_free };
            if chunk.free_head == NONE {
                self.partial.pop_front();
            }
            return (index, offset);
        }
        if self.open != NONE {
            let index = self.open as usize;
            let chunk = self.chunks[index].as_mut().expect("open chunk is live");
            let offset = chunk.bump;
            chunk.bump += 1;
            if chunk.bump as usize == Self::N {
                self.open = NONE;
            }
            return (index, offset);
        }
        let index = self
            .chunks
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.chunks.push(None);
                self.chunks.len() - 1
            });
        let mut chunk = Chunk::new(Self::N);
        chunk.bump = 1;
        self.chunks[index] = Some(chunk);
        self.open = index as u32;
        (index, 0)
    }

    /// Takes the value back out, releasing its slot for reuse.
    ///
    /// # Panics
    /// If `handle` does not name a live value.
    pub fn remove(&mut self, handle: u32) -> T {
        let (index, offset) = Self::split(handle);
        let chunk = self.chunks[index]
            .as_mut()
            .expect("handle names a live chunk");
        assert!(chunk.is_occupied(offset), "handle names a live value");
        let value = unsafe {
            ManuallyDrop::take(&mut chunk.slots[offset as usize].assume_init_mut().value)
        };
        chunk.set_occupied(offset, false);
        chunk.live -= 1;
        self.len -= 1;
        self.pending.push(handle);
        value
    }

    /// Hand every slot removed since the last call back for reuse, and free the
    /// chunks that emptied.
    ///
    /// The free lists are rebuilt ascending and served lowest chunk first, so a
    /// run of inserts takes handles in increasing order rather than in the
    /// order slots happened to die. That makes a batch of handles a function of
    /// the free set rather than of erase history; it does *not* make a recycled
    /// handle compare above a surviving one, so a caller that reads "allocated
    /// later" off "numbered higher" must not recycle at all.
    pub fn recycle(&mut self) {
        let mut touched: Vec<usize> = std::mem::take(&mut self.pending)
            .into_iter()
            .map(|handle| Self::split(handle).0)
            .collect();
        touched.sort_unstable();
        touched.dedup();
        for index in touched {
            let Some(chunk) = self.chunks[index].as_mut() else {
                continue;
            };
            if chunk.live == 0 && self.open != index as u32 {
                self.chunks[index] = None;
                continue;
            }
            chunk.free_head = NONE;
            for offset in (0..chunk.bump).rev() {
                if chunk.is_occupied(offset) {
                    continue;
                }
                let head = chunk.free_head;
                chunk.slots[offset as usize] = MaybeUninit::new(Slot { next_free: head });
                chunk.free_head = offset;
            }
        }
        self.partial = self
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, chunk)| chunk.as_ref().is_some_and(|c| c.free_head != NONE))
            .map(|(index, _)| index as u32)
            .collect();
    }

    pub fn get(&self, handle: u32) -> Option<&T> {
        let (index, offset) = Self::split(handle);
        let chunk = self.chunks.get(index)?.as_ref()?;
        chunk
            .is_occupied(offset)
            .then(|| unsafe { &*chunk.slots[offset as usize].assume_init_ref().value })
    }

    pub fn get_mut(&mut self, handle: u32) -> Option<&mut T> {
        let (index, offset) = Self::split(handle);
        let chunk = self.chunks.get_mut(index)?.as_mut()?;
        if !chunk.is_occupied(offset) {
            return None;
        }
        Some(unsafe { &mut *chunk.slots[offset as usize].assume_init_mut().value })
    }

    /// Live handles in ascending order.
    pub fn handles(&self) -> impl Iterator<Item = u32> + '_ {
        self.chunks
            .iter()
            .enumerate()
            .filter_map(|(index, chunk)| chunk.as_ref().map(|chunk| (index, chunk)))
            .flat_map(|(index, chunk)| {
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

    /// Chunks currently allocated, live or partially free.
    pub fn chunk_count(&self) -> usize {
        self.chunks.iter().filter(|chunk| chunk.is_some()).count()
    }

    /// Slots the allocated chunks hold, live or free.
    pub fn capacity(&self) -> usize {
        self.chunk_count() * Self::N
    }

    /// Bytes the chunks hold, whether or not their slots are live.
    pub fn bytes(&self) -> usize {
        self.chunk_count() * (Self::N * size_of::<Slot<T>>() + Self::N / 8)
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
        for chunk in self.chunks.iter_mut().flatten() {
            for offset in 0..chunk.bump {
                if chunk.is_occupied(offset) {
                    unsafe {
                        ManuallyDrop::drop(
                            &mut chunk.slots[offset as usize].assume_init_mut().value,
                        )
                    };
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
    fn removed_slots_are_reused_only_after_recycling() {
        let mut hive = Hive::new();
        let handles: Vec<_> = (0..4).map(|i| hive.insert(i as u64)).collect();
        hive.remove(handles[1]);
        hive.remove(handles[3]);
        assert!(!handles.contains(&hive.insert(99)));
        hive.recycle();
        assert_eq!(hive.insert(100), handles[1]);
        assert_eq!(hive.insert(101), handles[3]);
    }

    #[test]
    fn recycled_handles_are_handed_out_ascending() {
        let mut hive = Hive::new();
        let handles: Vec<_> = (0..8u64).map(|i| hive.insert(i)).collect();
        for handle in [handles[5], handles[1], handles[6], handles[2]] {
            hive.remove(handle);
        }
        hive.recycle();
        let reused: Vec<_> = (0..4).map(|i| hive.insert(100 + i)).collect();
        assert_eq!(reused, vec![handles[1], handles[2], handles[5], handles[6]]);
    }

    #[test]
    fn chunk_is_freed_when_every_slot_dies() {
        let mut hive: Hive<u64> = Hive::new();
        let handles: Vec<_> = (0..Hive::<u64>::N * 3)
            .map(|i| hive.insert(i as u64))
            .collect();
        assert_eq!(hive.chunk_count(), 3);
        for handle in &handles[..Hive::<u64>::N] {
            hive.remove(*handle);
        }
        assert_eq!(hive.chunk_count(), 3);
        hive.recycle();
        assert_eq!(hive.chunk_count(), 2);
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
                    hive.recycle();
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
