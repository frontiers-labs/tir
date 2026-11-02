use std::num::NonZeroUsize;

use tir_adt::CacheInfo;

// No row, in a slot and in the output of a batched lookup.
pub(crate) const EMPTY: u32 = u32::MAX;

// The L1 data cache, or 32 KiB where the platform does not report it: the
// smallest L1 data cache of the x86-64 and AArch64 cores in use.
fn l1_data() -> usize {
    CacheInfo::detect()
        .l1_data
        .map_or(32 << 10, NonZeroUsize::get)
}

// The L2 cache, or 256 KiB where the platform does not report it: the
// smallest L2 of the x86-64 and AArch64 cores in use.
pub(crate) fn l2() -> usize {
    CacheInfo::detect().l2.map_or(256 << 10, NonZeroUsize::get)
}

// Keys of `keys` cells a batched lookup probes together. A key touches a
// line of slots and a line per key cell, each at a random place, and these
// lines stay in half the L1 data cache from the first pass to the second;
// the other half holds what the caller streams.
pub(crate) fn probe_batch(keys: usize) -> usize {
    let line = CacheInfo::detect().line_size.get();
    (l1_data() / 2 / (line * (1 + keys))).max(1)
}

#[derive(Clone, Copy)]
struct Slot {
    hash: u32,
    id: u32,
}

// Open addressing over ids, such as row ids, whose keys live elsewhere. A
// slot stores the hash of its key, so growing never reads a key and a probe
// compares keys only on a hash match. The caller's `eq` compares the probed
// key with the key at an id. Linear probing; deletion moves later slots of
// the cluster back into the hole (Knuth's algorithm R), so there are no
// tombstones.
#[derive(Default)]
pub(crate) struct RowTable {
    slots: Vec<Slot>,
    len: usize,
}

impl RowTable {
    // Room for `len` ids before the first grow.
    fn with_capacity(len: usize) -> Self {
        let size = (len * 2).next_power_of_two().max(16);
        Self {
            slots: vec![Slot { hash: 0, id: EMPTY }; size],
            len: 0,
        }
    }

    pub(crate) fn get(&self, hash: u32, mut eq: impl FnMut(u32) -> bool) -> Option<u32> {
        let at = self.position(hash, &mut eq)?;
        Some(self.slots[at].id)
    }

    // The id of key `i` for each of `hashes`, or EMPTY, into `out`; `eq(i,
    // id)` compares key `i` with the key at `id`. The home slots of `batch`
    // keys load before any probe walks, so their cache misses overlap.
    pub(crate) fn get_many(
        &self,
        hashes: &[u32],
        mut eq: impl FnMut(usize, u32) -> bool,
        out: &mut [u32],
        batch: usize,
    ) {
        if self.slots.is_empty() {
            return out.fill(EMPTY);
        }
        let mask = self.slots.len() - 1;
        let mut homes = vec![Slot { hash: 0, id: EMPTY }; batch.min(hashes.len())];
        let chunks = hashes.chunks(batch).zip(out.chunks_mut(batch));
        for (at, (hashes, out)) in (0..).step_by(batch).zip(chunks) {
            for (home, &hash) in homes.iter_mut().zip(hashes) {
                *home = self.slots[hash as usize & mask];
            }
            for (i, ((&hash, id), home)) in
                hashes.iter().zip(out.iter_mut()).zip(&homes).enumerate()
            {
                let i = at + i;
                let mut next = hash as usize & mask;
                let mut slot = *home;
                *id = loop {
                    if slot.id == EMPTY {
                        break EMPTY;
                    }
                    if slot.hash == hash && eq(i, slot.id) {
                        break slot.id;
                    }
                    next = (next + 1) & mask;
                    slot = self.slots[next];
                };
            }
        }
    }

    // Returns the id the key had before.
    pub(crate) fn insert(
        &mut self,
        hash: u32,
        id: u32,
        mut eq: impl FnMut(u32) -> bool,
    ) -> Option<u32> {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = &mut self.slots[at];
            if slot.id == EMPTY {
                *slot = Slot { hash, id };
                self.len += 1;
                return None;
            }
            if slot.hash == hash && eq(slot.id) {
                return Some(std::mem::replace(&mut slot.id, id));
            }
            at = (at + 1) & mask;
        }
    }

    pub(crate) fn remove(&mut self, hash: u32, mut eq: impl FnMut(u32) -> bool) -> Option<u32> {
        let mut hole = self.position(hash, &mut eq)?;
        let id = self.slots[hole].id;
        let mask = self.slots.len() - 1;
        let mut next = hole;
        loop {
            next = (next + 1) & mask;
            let slot = self.slots[next];
            if slot.id == EMPTY {
                break;
            }
            // A slot whose home lies between the hole and itself stays put.
            let home_distance = next.wrapping_sub(slot.hash as usize) & mask;
            if home_distance >= (next.wrapping_sub(hole) & mask) {
                self.slots[hole] = slot;
                hole = next;
            }
        }
        self.slots[hole].id = EMPTY;
        self.len -= 1;
        Some(id)
    }

    fn position(&self, hash: u32, eq: &mut impl FnMut(u32) -> bool) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot.id == EMPTY {
                return None;
            }
            if slot.hash == hash && eq(slot.id) {
                return Some(at);
            }
            at = (at + 1) & mask;
        }
    }

    fn grow(&mut self) {
        let size = (self.slots.len() * 2).max(16);
        let old = std::mem::replace(&mut self.slots, vec![Slot { hash: 0, id: EMPTY }; size]);
        for slot in old.into_iter().filter(|slot| slot.id != EMPTY) {
            let mut at = slot.hash as usize & (size - 1);
            while self.slots[at].id != EMPTY {
                at = (at + 1) & (size - 1);
            }
            self.slots[at] = slot;
        }
    }
}

// Key -> row id of the live row with that key.
pub(crate) enum KeyMap {
    // A table with no key columns has at most one row.
    Unit(Option<u32>),
    // One key cell. `hash_row` of one cell is a bijection, so a slot whose
    // hash matches holds the key and a probe never reads the key column.
    Single(RowTable),
    Hashed(RowTable),
}

impl KeyMap {
    // For `rows` keys before the first grow.
    pub(crate) fn new(keys: usize, rows: usize) -> Self {
        match keys {
            0 => Self::Unit(None),
            1 => Self::Single(RowTable::with_capacity(rows)),
            _ => Self::Hashed(RowTable::with_capacity(rows)),
        }
    }

    pub(crate) fn get(&self, key: &[u32], hash: u32, columns: &[Vec<u32>]) -> Option<u32> {
        match self {
            Self::Unit(row) => *row,
            Self::Single(table) => table.get(hash, |_| true),
            Self::Hashed(table) => table.get(hash, |row| same_key(columns, row, key)),
        }
    }

    // `get` for a batch of keys given by column: key `i` is
    // `(keys[0][i], keys[1][i], ...)`, its hash `hashes[i]`, and its row or
    // EMPTY goes to `out[i]`.
    pub(crate) fn get_many(
        &self,
        keys: &[&[u32]],
        hashes: &[u32],
        columns: &[Vec<u32>],
        out: &mut [u32],
    ) {
        match self {
            Self::Unit(row) => out.fill(row.unwrap_or(EMPTY)),
            Self::Single(table) => table.get_many(hashes, |_, _| true, out, probe_batch(0)),
            Self::Hashed(table) => {
                let eq = |i: usize, row: u32| {
                    (keys.iter().zip(columns)).all(|(key, column)| column[row as usize] == key[i])
                };
                table.get_many(hashes, eq, out, probe_batch(keys.len()));
            }
        }
    }

    // Points the key at `row`, whose cells hold the key already.
    pub(crate) fn insert(&mut self, key: &[u32], hash: u32, row: u32, columns: &[Vec<u32>]) {
        match self {
            Self::Unit(slot) => *slot = Some(row),
            Self::Single(table) => {
                debug_assert!(
                    table
                        .get(hash, |old| !same_key(columns, old, key))
                        .is_none()
                );
                _ = table.insert(hash, row, |_| true);
            }
            Self::Hashed(table) => _ = table.insert(hash, row, |old| same_key(columns, old, key)),
        }
    }

    // Drops the key of `row`, which the map holds.
    pub(crate) fn remove(&mut self, row: u32, hash: u32) {
        match self {
            Self::Unit(slot) => *slot = None,
            Self::Single(table) | Self::Hashed(table) => _ = table.remove(hash, |id| id == row),
        }
    }
}

// Whether the row holds `key` in the first `key.len()` of `columns`.
pub(crate) fn same_key<C: AsRef<[u32]>>(columns: &[C], row: u32, key: &[u32]) -> bool {
    key.iter()
        .zip(columns)
        .all(|(&cell, column)| column.as_ref()[row as usize] == cell)
}
