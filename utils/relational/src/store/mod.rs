//! The generic relational layer: tables of `u32` cells with nothing in them
//! that knows what an e-graph is.
//!
//! A [`Table`] is a relation with a functional dependency: its key columns
//! determine its value column, and a hash index over the key enforces that. An
//! edge list is a table (`(source, label) -> target`), and so is an operator of
//! an e-graph (`children -> class`). What a client does when two rows come to
//! share a key is the client's business; the table reports the pair.
//!
//! A cell may carry a weight next to its id: an element of the group the id's
//! [`Shift::mask`] names, part of the key for a key cell. A table whose
//! weights are all zero stores none.
//!
//! Each row also carries a tag, a word the table stores and never interprets,
//! and a stamp, the epoch the row was last written in, which is what lets a
//! fixpoint evaluation join only against what changed.
//!
//! The loops over whole columns are [`tir_adt::simd`] kernels.

use tir_adt::simd::{self, hash_row};

/// Slot of the key index that never held a row.
const EMPTY: u32 = 0;
/// Slot whose row was removed; a probe walks past it.
const TOMBSTONE: u32 = u32::MAX;

/// How [`Table::repair`] moves weights along with ids: id `i` now stands at
/// `delta[i]` from the id its map sends it to, and a weight on an id `i` names
/// lives modulo `mask[i] + 1`. Both are indexed like the map.
#[derive(Clone, Copy, Debug)]
pub struct Shift<'a> {
    pub delta: &'a [u64],
    pub mask: &'a [u64],
}

/// A row [`Table::repair`] re-keyed, as its caller's `dissolve` sees it.
#[derive(Clone, Copy, Debug)]
pub struct RowView<'a> {
    pub cells: &'a [u32],
    /// One per key cell.
    pub weights: &'a [u64],
    pub value: (u32, u64),
    pub tag: u32,
}

/// Two rows [`Table::repair`] found under one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collision {
    /// The value and its weight of the row that stays.
    pub kept: (u32, u64),
    /// The value and its weight, and the tag, of the row that was removed.
    pub removed: (u32, u64),
    pub tag: u32,
}

/// What a [`Table::repair`] did, appended to whatever the lists already hold.
#[derive(Clone, Debug, Default)]
pub struct Repair {
    pub collisions: Vec<Collision>,
    /// The value of every row whose key moved, removed rows included.
    pub rekeyed: Vec<u32>,
}

/// A relation whose first `arity` columns determine the last.
#[derive(Clone, Debug)]
pub struct Table {
    arity: usize,
    /// `arity + 1` columns of equal length; the last is the value.
    columns: Vec<Vec<u32>>,
    /// The weight beside each cell of the column of the same index, or empty
    /// while every weight in the column is zero.
    weights: Vec<Vec<u64>>,
    /// Whether some column stores weights.
    weighted: bool,
    /// Per key column, how [`Self::repair`] carries the weight its cell picks
    /// up: zero keeps it in the key; any other coefficient moves it to the
    /// value, multiplied and negated, and the key cell keeps weight zero.
    coefficients: Vec<i64>,
    /// How many leading key columns [`Self::repair`] leaves as they are: cells
    /// that name something other than what the map renames.
    plain: usize,
    tag: Vec<u32>,
    stamp: Vec<u32>,
    /// Whether the key is unique. A table that is not keyed is a bag: it has no
    /// index and never reports a collision.
    keyed: bool,
    /// Open addressing over the key columns: `row + 1`, [`EMPTY`] or
    /// [`TOMBSTONE`] in the low half, the key's hash in the high half so a
    /// probe reads the columns only for a row that can match. Its length is a
    /// power of two.
    slots: Vec<u64>,
    /// Slots that are not [`EMPTY`].
    used: usize,
    /// Bumped by every change, so a reader can tell a derived index is stale.
    version: u64,
    key: Vec<u32>,
    key_weights: Vec<u64>,
    moved: Vec<u32>,
    dead: Vec<u32>,
}

/// What a key cell's weight adds to the key's hash: nothing for weight zero,
/// so an unweighted key hashes as [`hash_row`] alone.
fn weight_hash(column: usize, weight: u64) -> u32 {
    let mixed = weight
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .rotate_left(column as u32 * 7 + 17);
    (mixed ^ mixed >> 32) as u32
}

/// The hash a key of `cells` and `weights` is indexed under. `weights` is
/// empty or one per cell.
fn key_hash(cells: &[u32], weights: &[u64]) -> u32 {
    weights
        .iter()
        .enumerate()
        .fold(hash_row(cells), |hash, (column, &weight)| {
            hash ^ weight_hash(column, weight)
        })
}

impl Table {
    pub fn new(arity: usize) -> Self {
        Self {
            arity,
            columns: vec![Vec::new(); arity + 1],
            weights: vec![Vec::new(); arity + 1],
            weighted: false,
            coefficients: vec![0; arity],
            plain: 0,
            tag: Vec::new(),
            stamp: Vec::new(),
            keyed: true,
            slots: vec![EMPTY as u64; 8],
            used: 0,
            version: 0,
            key: vec![0; arity],
            key_weights: vec![0; arity],
            moved: Vec::new(),
            dead: Vec::new(),
        }
    }

    /// A table whose rows may repeat a key.
    pub fn bag(arity: usize) -> Self {
        Self {
            keyed: false,
            slots: Vec::new(),
            ..Self::new(arity)
        }
    }

    /// Leave the first `columns` key columns out of [`Self::repair`]'s rewrite.
    pub fn plain(mut self, columns: usize) -> Self {
        self.plain = columns;
        self
    }

    /// Give the key columns the coefficients [`Self::repair`] moves their
    /// weights to the value with; zero keeps a column's weight in the key.
    pub fn coefficients(mut self, coefficients: &[i64]) -> Self {
        debug_assert_eq!(coefficients.len(), self.arity);
        self.coefficients = coefficients.to_vec();
        self
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    pub fn len(&self) -> usize {
        self.stamp.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stamp.is_empty()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// One column, key columns first and the value column last.
    pub fn column(&self, index: usize) -> &[u32] {
        &self.columns[index]
    }

    pub fn values(&self) -> &[u32] {
        &self.columns[self.arity]
    }

    /// The weight beside the cell of `column` at `row`.
    #[inline]
    pub fn weight(&self, column: usize, row: u32) -> u64 {
        self.weights[column].get(row as usize).copied().unwrap_or(0)
    }

    /// Whether some cell carries a non-zero weight: a reader of a table
    /// without any can take every weight to be zero.
    #[inline]
    pub fn weighted(&self) -> bool {
        self.weighted
    }

    pub fn tags(&self) -> &[u32] {
        &self.tag
    }

    /// The epoch each row was last written in.
    pub fn stamps(&self) -> &[u32] {
        &self.stamp
    }

    fn set_weight(&mut self, column: usize, row: usize, weight: u64) {
        let weights = &mut self.weights[column];
        if weights.is_empty() {
            if weight == 0 {
                return;
            }
            weights.resize(self.stamp.len(), 0);
            self.weighted = true;
        }
        weights[row] = weight;
    }

    fn row_matches(&self, row: usize, key: &[u32], weights: &[u64]) -> bool {
        self.columns
            .iter()
            .zip(key)
            .all(|(column, &cell)| column[row] == cell)
            && (!self.weighted && weights.is_empty()
                || (0..self.arity).all(|column| {
                    self.weight(column, row as u32) == weights.get(column).copied().unwrap_or(0)
                }))
    }

    /// The row holding `key` at `weights`, which is empty for weight zero
    /// throughout or one per key cell. Always `None` for a bag.
    pub fn get(&self, key: &[u32], weights: &[u64]) -> Option<u32> {
        debug_assert_eq!(key.len(), self.arity);
        if !self.keyed {
            return None;
        }
        let hash = key_hash(key, weights);
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            match slot as u32 {
                EMPTY => return None,
                TOMBSTONE => {}
                row if (slot >> 32) as u32 == hash
                    && self.row_matches(row as usize - 1, key, weights) =>
                {
                    return Some(row - 1);
                }
                _ => {}
            }
            at = (at + 1) & mask;
        }
    }

    /// Append `key -> value`, with `weights` as for [`Self::get`] and
    /// `weight` beside the value. In a keyed table the key must not be
    /// present.
    pub fn insert(
        &mut self,
        key: &[u32],
        weights: &[u64],
        value: (u32, u64),
        tag: u32,
        stamp: u32,
    ) -> u32 {
        debug_assert!(self.get(key, weights).is_none(), "the key is new");
        if self.keyed {
            self.reserve(1);
        }
        let row = self.len() as u32;
        for (column, &cell) in self.columns.iter_mut().zip(key) {
            column.push(cell);
        }
        self.columns[self.arity].push(value.0);
        self.tag.push(tag);
        self.stamp.push(stamp);
        if self.weighted {
            for weights in &mut self.weights {
                if !weights.is_empty() {
                    weights.push(0);
                }
            }
        }
        if !weights.is_empty() || value.1 != 0 {
            for (column, &weight) in weights.iter().enumerate() {
                self.set_weight(column, row as usize, weight);
            }
            self.set_weight(self.arity, row as usize, value.1);
        }
        if self.keyed {
            self.place(row, key_hash(key, weights));
        }
        self.version += 1;
        row
    }

    /// Put `row` in the first free slot of its probe sequence.
    fn place(&mut self, row: u32, hash: u32) {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        while self.slots[at] as u32 != EMPTY && self.slots[at] as u32 != TOMBSTONE {
            at = (at + 1) & mask;
        }
        self.used += usize::from(self.slots[at] as u32 == EMPTY);
        self.slots[at] = (hash as u64) << 32 | (row + 1) as u64;
    }

    /// Make room for `extra` more entries with the index at most half full,
    /// which is what keeps a probe finite.
    fn reserve(&mut self, extra: usize) {
        if (self.used + extra) * 2 <= self.slots.len() {
            return;
        }
        let mut hashes = vec![0; self.len()];
        let keys: Vec<&[u32]> = self.columns[..self.arity]
            .iter()
            .map(Vec::as_slice)
            .collect();
        simd::hash_rows(&keys, &mut hashes);
        for (column, weights) in self.weights[..self.arity].iter().enumerate() {
            for (hash, &weight) in hashes.iter_mut().zip(weights) {
                *hash ^= weight_hash(column, weight);
            }
        }
        let size = ((self.len() + extra) * 4).next_power_of_two().max(8);
        self.slots.clear();
        self.slots.resize(size, EMPTY as u64);
        self.used = 0;
        // Rows in `moved` are out of the index until their caller puts them
        // back; the list is ascending.
        let mut moved = 0;
        for (row, hash) in hashes.into_iter().enumerate() {
            if self.moved.get(moved) == Some(&(row as u32)) {
                moved += 1;
                continue;
            }
            self.place(row as u32, hash);
        }
    }

    /// The loaded key's weights: empty for a table that has none.
    fn loaded_weights(&self) -> &[u64] {
        match self.weighted {
            true => &self.key_weights,
            false => &[],
        }
    }

    /// The slot that holds `row`, whose key is loaded.
    fn slot_of(&self, row: u32) -> usize {
        let mask = self.slots.len() - 1;
        let mut at = key_hash(&self.key, self.loaded_weights()) as usize & mask;
        while self.slots[at] as u32 != row + 1 {
            at = (at + 1) & mask;
        }
        at
    }

    fn load_key(&mut self, row: u32) {
        for (cell, column) in self.key.iter_mut().zip(&self.columns) {
            *cell = column[row as usize];
        }
        if self.weighted {
            for column in 0..self.arity {
                self.key_weights[column] = self.weight(column, row);
            }
        }
    }

    /// The rows any of whose cells in `columns` `map` moves, ascending, into
    /// `self.moved`.
    fn select_moved(&mut self, columns: std::ops::Range<usize>, map: &[u32]) {
        self.moved.clear();
        let several = columns.len() > 1;
        for column in columns {
            simd::select_moved(&self.columns[column], map, &mut self.moved);
        }
        if several {
            self.moved.sort_unstable();
            self.moved.dedup();
        }
    }

    /// Rewrite every key cell past the plain ones, and every value cell, through
    /// `map`, and restore the functional dependency.
    ///
    /// `map` sends a cell to the cell that now stands for it, and a cell that
    /// stands for itself to itself. With `shift`, a moved cell's weight is
    /// carried along: added to its own for a value or a key column without a
    /// coefficient, moved into the value's for one with. A row any of whose
    /// cells moved is stamped `stamp`. Where two rows end up with one key, the
    /// later one is removed and the pair is reported for the caller to
    /// reconcile. A re-keyed row `dissolve` answers true for is removed
    /// instead: the caller has taken its equation elsewhere. Row numbers are
    /// not stable across this call.
    ///
    /// Reports whether any cell moved.
    pub fn repair(
        &mut self,
        map: &[u32],
        shift: Option<Shift<'_>>,
        mut dissolve: Option<&mut dyn FnMut(RowView<'_>) -> bool>,
        stamp: u32,
        report: &mut Repair,
    ) -> bool {
        let arity = self.arity;
        self.select_moved(arity..arity + 1, map);
        let mut any = !self.moved.is_empty();
        for at in 0..self.moved.len() {
            let row = self.moved[at] as usize;
            let from = self.columns[arity][row];
            let to = map[from as usize];
            self.columns[arity][row] = to;
            if let Some(shift) = shift {
                let weight = self.weight(arity, row as u32);
                let weight =
                    weight.wrapping_add(shift.delta[from as usize]) & shift.mask[to as usize];
                self.set_weight(arity, row, weight);
            }
            self.stamp[row] = stamp;
        }
        self.select_moved(self.plain..arity, map);
        any |= !self.moved.is_empty();
        for at in 0..self.moved.len() {
            let row = self.moved[at];
            if self.keyed {
                // The index is keyed on the cells as they were, so the entry
                // goes before they change.
                self.load_key(row);
                let slot = self.slot_of(row);
                self.slots[slot] = TOMBSTONE as u64;
            }
            for column in self.plain..arity {
                let from = self.columns[column][row as usize];
                let to = map[from as usize];
                self.columns[column][row as usize] = to;
                let Some(shift) = shift else {
                    continue;
                };
                let delta = shift.delta[from as usize];
                if delta == 0 {
                    continue;
                }
                let mask = shift.mask[to as usize];
                match self.coefficients[column] {
                    0 => {
                        let weight = self.weight(column, row).wrapping_add(delta) & mask;
                        self.set_weight(column, row as usize, weight);
                    }
                    // op(.., to + delta, ..) == value, so op(.., to, ..) is
                    // value minus coefficient times delta.
                    coefficient => {
                        let moved = (coefficient as u64).wrapping_mul(delta);
                        let value = self.columns[arity][row as usize];
                        let weight = self.weight(arity, row).wrapping_sub(moved)
                            & shift.mask[value as usize];
                        self.set_weight(arity, row as usize, weight);
                    }
                }
            }
            self.stamp[row as usize] = stamp;
            report.rekeyed.push(self.columns[arity][row as usize]);
            if let Some(dissolve) = dissolve.as_mut() {
                self.load_key(row);
                let view = RowView {
                    cells: &self.key,
                    weights: &self.key_weights,
                    value: (self.columns[arity][row as usize], self.weight(arity, row)),
                    tag: self.tag[row as usize],
                };
                if dissolve(view) {
                    // Out of the index already, and stays out.
                    self.dead.push(row);
                }
            }
        }
        if self.keyed {
            self.reserve(self.moved.len());
            // The dissolved rows, ascending like the moved ones.
            let (dissolved, mut next) = (self.dead.len(), 0);
            for at in 0..self.moved.len() {
                let row = self.moved[at];
                if next < dissolved && self.dead[next] == row {
                    next += 1;
                    continue;
                }
                self.load_key(row);
                let key = std::mem::take(&mut self.key);
                let weights = std::mem::take(&mut self.key_weights);
                let weights_or_none: &[u64] = if self.weighted { &weights } else { &[] };
                match self.get(&key, weights_or_none) {
                    Some(kept) => {
                        report.collisions.push(Collision {
                            kept: (self.columns[arity][kept as usize], self.weight(arity, kept)),
                            removed: (self.columns[arity][row as usize], self.weight(arity, row)),
                            tag: self.tag[row as usize],
                        });
                        self.dead.push(row);
                    }
                    None => self.place(row, key_hash(&key, weights_or_none)),
                }
                self.key = key;
                self.key_weights = weights;
            }
        }
        self.moved.clear();
        // Highest first, so the row that fills a hole is never one still to go.
        self.dead.sort_unstable();
        for at in (0..self.dead.len()).rev() {
            self.swap_remove(self.dead[at]);
        }
        self.dead.clear();
        self.version += u64::from(any);
        any
    }

    /// Remove `row`, which the index does not hold, moving the last row into
    /// its place.
    fn swap_remove(&mut self, row: u32) {
        let last = self.len() as u32 - 1;
        if row != last && self.keyed {
            self.load_key(last);
            let slot = self.slot_of(last);
            self.slots[slot] = self.slots[slot] >> 32 << 32 | (row + 1) as u64;
        }
        for column in &mut self.columns {
            column.swap_remove(row as usize);
        }
        for weights in &mut self.weights {
            if !weights.is_empty() {
                weights.swap_remove(row as usize);
            }
        }
        self.tag.swap_remove(row as usize);
        self.stamp.swap_remove(row as usize);
    }
}

/// The rows of a table grouped by one column: `cell -> rows`, for the side of a
/// join that is looked up rather than scanned. Cells are dense small numbers,
/// so the grouping is a counting sort and a lookup is a slice.
#[derive(Clone, Debug, Default)]
pub struct Group {
    /// `offsets[cell]..offsets[cell + 1]` is the cell's range of `rows`.
    offsets: Vec<u32>,
    rows: Vec<u32>,
}

impl Group {
    /// Group the rows of `column`, every cell of which is below `cells`. Each
    /// group lists its rows ascending.
    pub fn build(&mut self, column: &[u32], cells: usize) {
        self.offsets.clear();
        self.offsets.resize(cells + 2, 0);
        for &cell in column {
            self.offsets[cell as usize + 2] += 1;
        }
        for cell in 2..self.offsets.len() {
            self.offsets[cell] += self.offsets[cell - 1];
        }
        self.rows.clear();
        self.rows.resize(column.len(), 0);
        // `offsets[cell + 1]` is the cell's cursor, and ends as its end.
        for (row, &cell) in column.iter().enumerate() {
            let cursor = &mut self.offsets[cell as usize + 1];
            self.rows[*cursor as usize] = row as u32;
            *cursor += 1;
        }
    }

    /// The rows holding `cell`, ascending.
    pub fn rows(&self, cell: u32) -> &[u32] {
        match self.offsets.get(cell as usize..cell as usize + 2) {
            Some(&[from, to]) => &self.rows[from as usize..to as usize],
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_key_is_found_after_the_index_grows() {
        let mut table = Table::new(2);
        for i in 0..100 {
            table.insert(&[i, i + 1], &[], (i * 10, 0), i, 1);
        }
        assert_eq!(table.get(&[7, 8], &[]), Some(7));
        assert_eq!(table.get(&[7, 9], &[]), None);
        assert_eq!(table.values()[7], 70);
    }

    #[test]
    fn a_table_with_no_key_columns_holds_one_row() {
        let mut table = Table::new(0);
        assert_eq!(table.get(&[], &[]), None);
        table.insert(&[], &[], (5, 0), 0, 1);
        assert_eq!(table.get(&[], &[]), Some(0));
    }

    #[test]
    fn repair_reports_rows_that_come_to_share_a_key() {
        let mut table = Table::new(1);
        table.insert(&[0], &[], (10, 0), 100, 1);
        table.insert(&[1], &[], (11, 0), 101, 1);
        table.insert(&[2], &[], (12, 0), 102, 1);
        // Cell 1 now stands for cell 0, so the first two rows collide.
        let map: Vec<u32> = (0..13)
            .map(|cell| if cell == 1 { 0 } else { cell })
            .collect();
        let mut report = Repair::default();
        assert!(table.repair(&map, None, None, 2, &mut report));
        assert_eq!(
            report.collisions,
            vec![Collision {
                kept: (10, 0),
                removed: (11, 0),
                tag: 101
            }]
        );
        assert_eq!(report.rekeyed, vec![11]);
        assert_eq!(table.len(), 2);
        let value = |key: u32| {
            table
                .get(&[key], &[])
                .map(|row| table.values()[row as usize])
        };
        assert_eq!((value(0), value(1), value(2)), (Some(10), None, Some(12)));
    }

    /// `f(a, b)` with a coefficient on `a` and none on `b`: a weight `a` picks
    /// up moves into the value, one `b` picks up stays in the key.
    #[test]
    fn repair_moves_a_coefficient_columns_weight_into_the_value() {
        let mut table = Table::new(2).coefficients(&[1, 0]);
        table.insert(&[1, 2], &[], (5, 0), 0, 1);
        table.insert(&[0, 3], &[0, 1], (6, 0), 1, 1);
        // 1 is 0 + 3 and 2 is 3 + 1, modulo 2^8.
        let map = [0, 0, 3, 3, 4, 5, 6];
        let delta = [0, 3, 1, 0, 0, 0, 0];
        let mask = [0xff; 7];
        let shift = Shift {
            delta: &delta,
            mask: &mask,
        };
        let mut report = Repair::default();
        table.repair(&map, Some(shift), None, 2, &mut report);
        // f(0 + 3, 3 + 1) = 5, so f(0, 3 + 1) = 5 - 3, which the second row
        // says is 6.
        assert_eq!(
            report.collisions,
            vec![Collision {
                kept: (6, 0),
                removed: (5, 253),
                tag: 0
            }]
        );
        assert_eq!(table.get(&[0, 3], &[0, 1]), Some(0));
        assert_eq!(table.get(&[0, 3], &[]), None);
    }

    #[test]
    fn a_bag_keeps_rows_that_come_to_share_a_key() {
        let mut table = Table::bag(1);
        table.insert(&[0], &[], (10, 0), 0, 1);
        table.insert(&[1], &[], (11, 0), 1, 1);
        let mut report = Repair::default();
        assert!(table.repair(
            &[0, 0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            None,
            None,
            2,
            &mut report
        ));
        assert!(report.collisions.is_empty());
        assert_eq!(table.column(0), &[0, 0]);
        assert_eq!(table.stamps(), &[1, 2]);
    }

    proptest! {
        /// Repair against a model: the surviving keys are the mapped keys, each
        /// once, and every one is reachable through the index.
        #[test]
        fn repair_leaves_one_row_per_mapped_key(
            keys in prop::collection::btree_set((0u32..12, 0u32..12), 0..60),
            map in prop::collection::vec(0u32..12, 12),
        ) {
            // Make the map idempotent, as a flattened union-find is.
            let map: Vec<u32> = (0..12).map(|cell| {
                let mut at = cell;
                for _ in 0..12 { at = at.min(map[at as usize]); }
                at
            }).collect();
            let map: Vec<u32> = map.iter().map(|&cell| map[cell as usize]).collect();
            prop_assume!(map.iter().all(|&cell| map[cell as usize] == cell));
            let mut table = Table::new(2);
            for (tag, &(a, b)) in keys.iter().enumerate() {
                table.insert(&[a, b], &[], (0, 0), tag as u32, 1);
            }
            let mut report = Repair::default();
            table.repair(&map, None, None, 2, &mut report);
            let expected: std::collections::BTreeSet<(u32, u32)> =
                keys.iter().map(|&(a, b)| (map[a as usize], map[b as usize])).collect();
            let found: std::collections::BTreeSet<(u32, u32)> =
                (0..table.len()).map(|row| (table.column(0)[row], table.column(1)[row])).collect();
            prop_assert_eq!(table.len(), expected.len());
            prop_assert_eq!(report.collisions.len(), keys.len() - expected.len());
            for &(a, b) in &expected {
                let row = table.get(&[a, b], &[]);
                prop_assert!(row.is_some());
                prop_assert_eq!((table.column(0)[row.unwrap() as usize], table.column(1)[row.unwrap() as usize]), (a, b));
            }
            prop_assert_eq!(found, expected);
        }

        #[test]
        fn a_group_lists_each_cells_rows_ascending(column in prop::collection::vec(0u32..6, 0..80)) {
            let mut group = Group::default();
            group.build(&column, 6);
            for cell in 0..8 {
                let expected: Vec<u32> = (0..column.len() as u32).filter(|&row| column[row as usize] == cell).collect();
                prop_assert_eq!(group.rows(cell), &expected[..]);
            }
        }
    }
}
