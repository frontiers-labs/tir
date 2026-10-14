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
//! [`Rename::shift`] names, part of the key for a key cell. A table whose
//! weights are all zero stores none.
//!
//! Each row also carries a tag, a word the table stores and never interprets,
//! and a stamp, the epoch the row was last written in, which is what lets a
//! fixpoint evaluation join only against what changed.
//!
//! The loops over whole columns are [`tir_adt::simd`] kernels.

use tir_adt::simd::{self, hash_row};

/// An index places the rows appended or moved since its base was built until
/// they, with the base entries they left behind, outnumber this fraction of
/// the base, and then rebuilds the base.
pub(crate) const REBUILD_DIVISOR: usize = 8;

const FIRST_ROWS: usize = 64;

/// Slot of the key index that never held a row.
const EMPTY: u32 = 0;
/// Slot whose row was removed; a probe walks past it.
const TOMBSTONE: u32 = u32::MAX;

/// How [`Table::repair`] renames cells: each cell to the cell that now stands
/// for it, and the offset it stands at from it.
pub trait Rename {
    /// The rows of `column` whose cell the rename moves, ascending, into `out`.
    fn select_moved(&self, column: &[u32], out: &mut Vec<u32>);
    /// The cell that stands for `cell`; `cell` itself when nothing does.
    fn to(&self, cell: u32) -> u32;
    /// The offset `cell` sits at from [`Self::to`] and the mask weights on the
    /// destination live modulo, or `None` while no cell sits at an offset.
    fn shift(&self, cell: u32) -> Option<(u64, u64)>;
}

/// A rename by a flattened map over every id: id `i` stands at `delta[i]`
/// from `map[i]`, and a weight on id `i` lives modulo `mask[i] + 1`.
#[derive(Clone, Copy, Debug)]
pub struct Dense<'a> {
    pub map: &'a [u32],
    /// `None` while every delta is zero.
    pub delta: Option<&'a [u64]>,
    pub mask: &'a [u64],
}

impl Rename for Dense<'_> {
    #[inline]
    fn select_moved(&self, column: &[u32], out: &mut Vec<u32>) {
        simd::select_moved(column, self.map, out);
    }

    #[inline]
    fn to(&self, cell: u32) -> u32 {
        self.map[cell as usize]
    }

    #[inline]
    fn shift(&self, cell: u32) -> Option<(u64, u64)> {
        let delta = self.delta?;
        Some((
            delta[cell as usize],
            self.mask[self.map[cell as usize] as usize],
        ))
    }
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

/// The values of the rows [`Table::probe_or_insert`] stores.
#[derive(Clone, Copy, Debug)]
pub enum Values<'a> {
    /// Key `index` of the batch is stored with `values[index]`.
    Each(&'a [u32]),
    /// The `n`-th row stored gets the value `first + n`.
    From(u32),
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
    // Two key columns whose cells are an unordered pair: every row holds
    // the lesser `(cell, weight)` in the first.
    unordered: Option<[usize; 2]>,
    tag: Vec<u32>,
    stamp: Vec<u32>,
    // The highest stamp a row was given since the table was last empty.
    stamped: u32,
    // Every row written since `mark` was called sits at or past this one.
    marked: usize,
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
    /// The rows every [`Self::repair`] rewrote or moved another row into,
    /// oldest first, with per repair the version it left the table at, where
    /// its rows start and the length it left the table at; and the version
    /// the log is complete from. The log is what lets an index merge a repair
    /// instead of rebuilding.
    touched: Vec<u32>,
    repairs: Vec<(u64, u32, u32)>,
    floor: u64,
    /// Bumped by every [`Self::set_tag`], which no other index reads.
    retagged: u64,
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
            unordered: None,
            tag: Vec::new(),
            stamp: Vec::new(),
            stamped: 0,
            marked: 0,
            keyed: true,
            slots: vec![EMPTY as u64; 8],
            used: 0,
            version: 0,
            touched: Vec::new(),
            repairs: Vec::new(),
            floor: 0,
            retagged: 0,
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

    /// Remove every row, keeping what the table allocated.
    pub fn clear(&mut self) {
        for column in &mut self.columns {
            column.clear();
        }
        for weights in &mut self.weights {
            weights.clear();
        }
        self.weighted = false;
        self.tag.clear();
        self.stamp.clear();
        self.stamped = 0;
        self.marked = 0;
        self.slots.fill(EMPTY as u64);
        self.used = 0;
        self.version += 1;
        self.touched.clear();
        self.repairs.clear();
        self.floor = self.version;
        self.moved.clear();
        self.dead.clear();
    }

    /// An empty table of the same shape: arity, key, plain columns and
    /// coefficients.
    pub fn like(&self) -> Self {
        let table = match self.keyed {
            true => Self::new(self.arity),
            false => Self::bag(self.arity),
        };
        let table = table.plain(self.plain).coefficients(&self.coefficients);
        match self.unordered {
            Some(columns) => table.unordered(columns),
            None => table,
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

    /// Make key columns `columns` an unordered pair: a key is stored and
    /// looked up with the lesser `(cell, weight)` first, which
    /// [`Self::order`] puts it in and [`Self::repair`] keeps it in. Both
    /// columns have one coefficient.
    pub fn unordered(mut self, columns: [usize; 2]) -> Self {
        debug_assert_eq!(self.coefficients[columns[0]], self.coefficients[columns[1]]);
        self.unordered = Some(columns);
        self
    }

    /// Put a key of this table, `weights` one per cell, in the order its
    /// unordered pair is stored in. Reports whether the pair was exchanged.
    #[inline]
    pub fn order(&self, cells: &mut [u32], weights: &mut [u64]) -> bool {
        let Some([a, b]) = self.unordered else {
            return false;
        };
        let exchange = (cells[a], weights[a]) > (cells[b], weights[b]);
        if exchange {
            cells.swap(a, b);
            weights.swap(a, b);
        }
        exchange
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    /// How many leading key columns hold cells [`Self::repair`] leaves alone.
    pub fn plain_columns(&self) -> usize {
        self.plain
    }

    /// The coefficient [`Self::repair`] moves `column`'s weight to the value
    /// with; zero keeps it in the key.
    pub fn coefficient(&self, column: usize) -> i64 {
        self.coefficients[column]
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

    /// The rows the repairs since the table was at `version` rewrote or
    /// moved another row into, and the fewest rows the table has had since;
    /// `None` when the table no longer remembers that far back.
    pub fn moved_since(&self, version: u64) -> Option<(&[u32], usize)> {
        if version < self.floor {
            return None;
        }
        let at = self
            .repairs
            .partition_point(|&(after, ..)| after <= version);
        let since = &self.repairs[at..];
        let start = since
            .first()
            .map_or(self.touched.len(), |&(_, start, _)| start as usize);
        let shortest = since.iter().map(|&(.., len)| len as usize).min();
        Some((&self.touched[start..], shortest.unwrap_or(self.len())))
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

    /// Replace the tag of `row`.
    pub fn set_tag(&mut self, row: u32, tag: u32) {
        self.tag[row as usize] = tag;
        self.retagged += 1;
    }

    /// The epoch each row was last written in.
    pub fn stamps(&self) -> &[u32] {
        &self.stamp
    }

    /// The latest epoch a row was written in: no row is stamped above it.
    pub fn stamped(&self) -> u32 {
        self.stamped
    }

    /// Mark the rows the table has now as old: [`Self::repair`] can be told
    /// to look only at the rows written since.
    pub fn mark(&mut self) {
        self.marked = self.len();
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

    fn hash_keys(keys: &[&[u32]], weights: &[&[u64]], hashes: &mut Vec<u32>) {
        hashes.clear();
        hashes.resize(keys.first().map_or(0, |column| column.len()), 0);
        simd::hash_rows(keys, hashes);
        for (column, weights) in weights.iter().enumerate() {
            for (hash, &weight) in hashes.iter_mut().zip(*weights) {
                *hash ^= weight_hash(column, weight);
            }
        }
    }

    // The row holding key `index` of a batch, or the empty slot its walk
    // from `hash` ends at.
    #[inline]
    fn walk(
        &self,
        hash: u32,
        keys: &[&[u32]],
        weights: &[&[u64]],
        index: usize,
    ) -> Result<u32, usize> {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            match slot as u32 {
                EMPTY => return Err(at),
                TOMBSTONE => {}
                row if (slot >> 32) as u32 == hash
                    && (0..self.arity).all(|column| {
                        self.columns[column][row as usize - 1] == keys[column][index]
                            && self.weight(column, row - 1)
                                == weights.get(column).map_or(0, |weights| weights[index])
                    }) =>
                {
                    return Ok(row - 1);
                }
                _ => {}
            }
            at = (at + 1) & mask;
        }
    }

    /// The row holding each key of a batch, or `u32::MAX` where the table has
    /// none. `keys` is one slice per key column, all of one length, and
    /// `weights` is empty or the same of the weights beside them. The hashes
    /// of the whole batch are computed column by column before any slot is
    /// read. A bag holds no key.
    pub fn probe(&self, keys: &[&[u32]], weights: &[&[u64]], rows: &mut Vec<u32>) {
        debug_assert_eq!(keys.len(), self.arity);
        Self::hash_keys(keys, weights, rows);
        if !self.keyed {
            rows.fill(u32::MAX);
            return;
        }
        for (index, found) in rows.iter_mut().enumerate() {
            *found = self.walk(*found, keys, weights, index).unwrap_or(u32::MAX);
        }
    }

    /// [`Self::probe`] that stores each key the table has none of as a row,
    /// stamped `stamp`: the `n`-th row stored is tagged `tag + n` and valued
    /// as `values` says. `rows[index]` is `u32::MAX` for a key stored and
    /// otherwise the row holding it, which a key earlier in the batch may
    /// have stored. The batch is hashed column by column once and each key
    /// walks the slots once. A bag stores every key.
    pub fn probe_or_insert(
        &mut self,
        keys: &[&[u32]],
        weights: &[&[u64]],
        values: Values<'_>,
        tag: u32,
        stamp: u32,
        rows: &mut Vec<u32>,
    ) {
        debug_assert_eq!(keys.len(), self.arity);
        let arity = self.arity;
        Self::hash_keys(keys, weights, rows);
        if self.keyed {
            self.reserve(rows.len());
        }
        let mut stored = 0;
        for (index, found) in rows.iter_mut().enumerate() {
            let hash = *found;
            let free = match self.keyed {
                true => match self.walk(hash, keys, weights, index) {
                    Ok(row) => {
                        *found = row;
                        continue;
                    }
                    Err(free) => Some(free),
                },
                false => None,
            };
            *found = u32::MAX;
            let row = self.stamp.len();
            for (column, key) in self.columns.iter_mut().zip(keys) {
                column.push(key[index]);
            }
            self.columns[arity].push(match values {
                Values::Each(values) => values[index],
                Values::From(first) => first + stored,
            });
            self.tag.push(tag + stored);
            self.stamp.push(stamp);
            for weights in &mut self.weights {
                if !weights.is_empty() {
                    weights.push(0);
                }
            }
            for (column, weights) in weights.iter().enumerate() {
                self.set_weight(column, row, weights[index]);
            }
            if let Some(free) = free {
                self.slots[free] = (hash as u64) << 32 | (row + 1) as u64;
                self.used += 1;
            }
            stored += 1;
        }
        if stored != 0 {
            self.version += 1;
            self.stamped = self.stamped.max(stamp);
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
        if self.tag.len() == self.tag.capacity() {
            self.grow_rows();
        }
        let row = self.len() as u32;
        for (column, &cell) in self.columns.iter_mut().zip(key) {
            column.push(cell);
        }
        self.columns[self.arity].push(value.0);
        self.tag.push(tag);
        self.stamp.push(stamp);
        self.stamped = self.stamped.max(stamp);
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

    // Room for as many rows again, in every column at once: a table's first
    // rows would otherwise grow each column several times over.
    fn grow_rows(&mut self) {
        let more = self.tag.len().max(FIRST_ROWS);
        for column in &mut self.columns {
            column.reserve(more);
        }
        self.tag.reserve(more);
        self.stamp.reserve(more);
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

    /// The rows from `from` on any of whose cells in `columns` `map` moves,
    /// ascending, into `self.moved`.
    fn select_moved(&mut self, columns: std::ops::Range<usize>, from: usize, map: &impl Rename) {
        self.moved.clear();
        let several = columns.len() > 1;
        for column in columns {
            map.select_moved(&self.columns[column][from..], &mut self.moved);
        }
        for row in &mut self.moved {
            *row += from as u32;
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
    /// With `recent`, `map` moves no cell of a row written before the last
    /// [`Self::mark`], and those rows are not read.
    ///
    /// Reports whether any cell moved.
    pub fn repair(
        &mut self,
        map: &impl Rename,
        mut dissolve: Option<&mut dyn FnMut(RowView<'_>) -> bool>,
        stamp: u32,
        recent: bool,
        report: &mut Repair,
    ) -> bool {
        let arity = self.arity;
        let start = self.touched.len();
        let from = match recent {
            true => self.marked.min(self.len()),
            false => 0,
        };
        self.select_moved(arity..arity + 1, from, map);
        let mut any = !self.moved.is_empty();
        for at in 0..self.moved.len() {
            let row = self.moved[at] as usize;
            let from = self.columns[arity][row];
            self.columns[arity][row] = map.to(from);
            if let Some((delta, mask)) = map.shift(from) {
                let weight = self.weight(arity, row as u32).wrapping_add(delta) & mask;
                self.set_weight(arity, row, weight);
            }
            self.stamp[row] = stamp;
        }
        self.touched.extend_from_slice(&self.moved);
        let mut logged = start..self.touched.len();
        self.select_moved(self.plain..arity, from, map);
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
                self.columns[column][row as usize] = map.to(from);
                let Some((delta, mask)) = map.shift(from) else {
                    continue;
                };
                if delta == 0 {
                    continue;
                }
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
                        let (_, mask) = map.shift(value).unwrap_or((0, u64::MAX));
                        let weight = self.weight(arity, row).wrapping_sub(moved) & mask;
                        self.set_weight(arity, row as usize, weight);
                    }
                }
            }
            if let Some([a, b]) = self.unordered {
                let at = row as usize;
                let first = (self.columns[a][at], self.weight(a, row));
                let second = (self.columns[b][at], self.weight(b, row));
                if first > second {
                    (self.columns[a][at], self.columns[b][at]) = (second.0, first.0);
                    self.set_weight(a, at, second.1);
                    self.set_weight(b, at, first.1);
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
                    None => {
                        self.place(row, key_hash(&key, weights_or_none));
                        self.touch(row, &mut logged);
                    }
                }
                self.key = key;
                self.key_weights = weights;
            }
        } else {
            for at in 0..self.moved.len() {
                self.touch(self.moved[at], &mut logged);
            }
        }
        self.moved.clear();
        // Highest first, so the row that fills a hole is never one still to go.
        self.dead.sort_unstable();
        for at in (0..self.dead.len()).rev() {
            self.swap_remove(self.dead[at]);
        }
        self.dead.clear();
        if any {
            self.version += 1;
            self.stamped = self.stamped.max(stamp);
            self.repairs
                .push((self.version, start as u32, self.len() as u32));
            // A log longer than the delta an index rebuilds at serves no one.
            if self.touched.len() > self.len() / REBUILD_DIVISOR {
                self.touched.clear();
                self.repairs.clear();
                self.floor = self.version;
            }
        }
        any
    }

    /// Log `row` as touched unless the value phase did, whose rows `logged`
    /// walks: both ascend.
    fn touch(&mut self, row: u32, logged: &mut std::ops::Range<usize>) {
        while logged.start < logged.end && self.touched[logged.start] < row {
            logged.start += 1;
        }
        if logged.start == logged.end || self.touched[logged.start] != row {
            self.touched.push(row);
        }
    }

    /// Remove `row`, which the index does not hold, moving the last row into
    /// its place.
    fn swap_remove(&mut self, row: u32) {
        let last = self.len() as u32 - 1;
        // The row that fills the hole may be one written since the mark.
        self.marked = self.marked.min(row as usize);
        if row != last && self.keyed {
            self.load_key(last);
            let slot = self.slot_of(last);
            self.slots[slot] = self.slots[slot] >> 32 << 32 | (row + 1) as u64;
            self.touched.push(row);
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

/// What an index keeps beside its base, built over the first rows of a
/// table: which rows changed since, for the index to place again, and how
/// much it has placed and dropped, which decides when the base is rebuilt.
#[derive(Clone, Debug, Default)]
pub(crate) struct Delta {
    /// The rows the last refresh found changed, ascending, and the rows the
    /// table dropped since the one before.
    pub(crate) changed: Vec<u32>,
    pub(crate) dropped: std::ops::Range<u32>,
    /// The table's version and length at the last refresh.
    version: u64,
    seen: usize,
    built: bool,
    /// How many rows the base was built over, and how many were placed or
    /// dropped since.
    base: usize,
    merged: usize,
}

/// What [`Delta::refresh`] found to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refresh {
    /// The table is as it was.
    Current,
    /// The changed rows are to leave where they sit and be placed again under
    /// their cells now, and the dropped rows to leave.
    Merged,
    /// The base is to be built over the table again.
    Rebuild,
}

/// Where a row sits in an index when not at a position of its base.
pub(crate) const ABSENT: u32 = u32::MAX;
pub(crate) const TAIL: u32 = u32::MAX - 1;

impl Delta {
    /// Bring the delta up to date with `table`.
    pub(crate) fn refresh(&mut self, table: &Table) -> Refresh {
        let len = table.len();
        if self.built && self.version == table.version() {
            return Refresh::Current;
        }
        let moved = self
            .built
            .then(|| table.moved_since(self.version))
            .flatten();
        self.version = table.version();
        let seen = std::mem::replace(&mut self.seen, len);
        if let Some((touched, shortest)) = moved {
            // The rows repairs touched and the rows appended since, each
            // once.
            self.changed.clear();
            self.changed
                .extend(touched.iter().filter(|&&row| (row as usize) < len));
            self.changed.extend(shortest.min(seen) as u32..len as u32);
            self.changed.sort_unstable();
            self.changed.dedup();
            self.dropped = len as u32..seen.max(len) as u32;
            self.merged += self.changed.len() + self.dropped.len();
            if self.merged <= self.base / REBUILD_DIVISOR {
                return Refresh::Merged;
            }
        }
        Refresh::Rebuild
    }

    /// Note the base built over `base` rows.
    pub(crate) fn rebuilt(&mut self, base: usize) {
        self.built = true;
        self.base = base;
        self.merged = 0;
        self.changed.clear();
        self.dropped = 0..0;
    }

    /// The rows that leave where they sit: the changed and the dropped.
    pub(crate) fn leaving(&self) -> impl Iterator<Item = u32> + '_ {
        self.changed.iter().copied().chain(self.dropped.clone())
    }
}

/// Take `row` out of a base whose entries lie in the ascending runs
/// `rows[from..to]` of `runs`, `pos` saying where each row sits.
pub(crate) fn drop_row(rows: &mut [u32], pos: &mut [u32], runs: &mut [(u32, u32)], row: u32) {
    let at = pos[row as usize];
    let run = runs.partition_point(|&(from, _)| from <= at) - 1;
    let (at, end) = (at as usize, runs[run].1 as usize);
    pos[row as usize] = ABSENT;
    rows.copy_within(at + 1..end, at);
    for &row in &rows[at..end - 1] {
        pos[row as usize] -= 1;
    }
    runs[run].1 -= 1;
}

// Runs of a grouping by a plain column whose cells run past this many times
// its rows are not kept: laying them out costs a pass over the cells, most of
// them empty.
const SPARSE: usize = 8;

/// The rows of a table grouped by one column: `cell -> rows`, for the side of a
/// join that is looked up rather than scanned. Cells are dense small numbers,
/// so the grouping is a counting sort and a lookup is a slice.
///
/// The counting sort is a base over the rows the table had when it was built,
/// each cell's run left some slack. A row that changes since is placed in its
/// cell's run, or in a tail sorted by cell when the cell has no run; a run
/// that fills up has the base rebuilt. A base over a plain column whose cells
/// are sparse has no runs, and every row in the tail.
#[derive(Clone, Debug, Default)]
pub struct Group {
    /// `rows[from..to]` is the cell's run for `ranges[cell] == (from, to)`,
    /// which may grow to the next cell's `from`; the last entry closes the
    /// last run. `pos[row]` is where a row sits, [`TAIL`] or [`ABSENT`].
    ranges: Vec<(u32, u32)>,
    rows: Vec<u32>,
    pos: Vec<u32>,
    live: usize,
    delta: Delta,
    /// The tail's rows sorted by cell, then ascending, their cells, and a bit
    /// per cell that has any.
    tail: Vec<u32>,
    cells: Vec<u32>,
    has: Vec<u64>,
}

impl Group {
    /// Bring the grouping of `table` by `column` up to date. Cells of a column
    /// past the plain ones are below `ids`.
    pub fn refresh(&mut self, table: &Table, column: usize, ids: usize) {
        let cells = table.column(column);
        match self.delta.refresh(table) {
            Refresh::Current => return,
            Refresh::Merged if self.place(cells) => return,
            _ => {}
        }
        let plain = column < table.plain_columns();
        let bound = match plain {
            true => cells.iter().max().map_or(0, |&max| max as usize + 1),
            false => ids,
        };
        self.tail.clear();
        if plain && bound > cells.len().saturating_mul(SPARSE) {
            // Runs would be mostly empty: every row goes in the tail.
            self.ranges.clear();
            self.rows.clear();
            self.pos.clear();
            self.pos.resize(cells.len(), TAIL);
            self.live = 0;
            self.tail.extend(0..cells.len() as u32);
            self.index_tail(cells);
            self.delta.rebuilt(cells.len());
            return;
        }
        self.ranges.clear();
        self.ranges.resize(bound + 1, (0, 0));
        for &cell in cells {
            self.ranges[cell as usize].1 += 1;
        }
        // Each run gets an eighth and one of slack.
        let mut sum = 0;
        for range in &mut self.ranges {
            let count = range.1;
            *range = (sum, sum);
            sum += count + if count == 0 { 0 } else { count / 8 + 1 };
        }
        self.rows.clear();
        self.rows.resize(sum as usize, 0);
        self.pos.clear();
        self.pos.resize(cells.len(), ABSENT);
        // A cell's end is its cursor, and ends as its end.
        for (row, &cell) in cells.iter().enumerate() {
            let cursor = &mut self.ranges[cell as usize].1;
            self.rows[*cursor as usize] = row as u32;
            self.pos[row] = *cursor;
            *cursor += 1;
        }
        self.live = cells.len();
        self.cells.clear();
        self.has.clear();
        self.delta.rebuilt(cells.len());
    }

    // Sort the tail by cell and row, and list its cells.
    fn index_tail(&mut self, cells: &[u32]) {
        self.tail
            .sort_unstable_by_key(|&row| (cells[row as usize], row));
        self.cells.clear();
        self.cells
            .extend(self.tail.iter().map(|&row| cells[row as usize]));
        self.has.clear();
        let top = self.cells.last().map_or(0, |&cell| cell as usize + 1);
        self.has.resize(top.div_ceil(64), 0);
        for &cell in &self.cells {
            self.has[cell as usize / 64] |= 1 << (cell % 64);
        }
    }

    /// Place the changed rows again; `false` when a run is full, which has
    /// the base rebuilt.
    fn place(&mut self, cells: &[u32]) -> bool {
        if self.pos.len() < cells.len() {
            self.pos.resize(cells.len(), ABSENT);
        }
        for row in self.delta.leaving() {
            match self.pos[row as usize] {
                ABSENT => {}
                TAIL => self.pos[row as usize] = ABSENT,
                _ => {
                    drop_row(&mut self.rows, &mut self.pos, &mut self.ranges, row);
                    self.live -= 1;
                }
            }
        }
        let pos = &self.pos;
        self.tail.retain(|&row| pos[row as usize] == TAIL);
        for &row in &self.delta.changed {
            let cell = cells[row as usize] as usize;
            let (from, to) = self.ranges.get(cell).copied().unwrap_or((0, 0));
            let room = self.ranges.get(cell + 1).map_or(0, |next| next.0);
            if to < room {
                let run = &self.rows[from as usize..to as usize];
                let at = from as usize + run.partition_point(|&other| other < row);
                self.rows.copy_within(at..to as usize, at + 1);
                self.rows[at] = row;
                for &moved in &self.rows[at + 1..=to as usize] {
                    self.pos[moved as usize] += 1;
                }
                self.pos[row as usize] = at as u32;
                self.ranges[cell].1 += 1;
                self.live += 1;
            } else if from != to {
                return false;
            } else {
                self.tail.push(row);
                self.pos[row as usize] = TAIL;
            }
        }
        self.index_tail(cells);
        true
    }

    /// The rows holding `cell`, ascending.
    #[inline(always)]
    pub fn rows(&self, cell: u32) -> &[u32] {
        match self.ranges.get(cell as usize) {
            Some(&(from, to)) if from != to => &self.rows[from as usize..to as usize],
            _ => match self.has.get(cell as usize / 64) {
                Some(word) if word >> (cell % 64) & 1 != 0 => self.tail_of(cell),
                _ => &[],
            },
        }
    }

    #[inline(never)]
    fn tail_of(&self, cell: u32) -> &[u32] {
        let from = self.cells.partition_point(|&other| other < cell);
        let to = from + self.cells[from..].partition_point(|&other| other == cell);
        &self.tail[from..to]
    }

    /// How many cells the grouping has room for: one past the highest.
    pub fn cells(&self) -> usize {
        let tail = self.cells.last().map_or(0, |&cell| cell as usize + 1);
        self.ranges.len().saturating_sub(1).max(tail)
    }

    /// How many rows the cells have, all together.
    pub fn len(&self) -> usize {
        self.live + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The rows of a table ordered by the cell of one column and then by tag, so
/// the rows holding one cell with a tag in an interval are one slice. Built
/// again whenever the table's rows or tags change.
#[derive(Clone, Debug, Default)]
pub struct TagOrder {
    /// The table's version and retag count the order was built at.
    made: Option<(u64, u64)>,
    /// Per row in order, its cell in the high half and its tag in the low.
    keys: Vec<u64>,
    rows: Vec<u32>,
}

impl TagOrder {
    /// Bring the order of `table` by `column` up to date.
    pub fn refresh(&mut self, table: &Table, column: usize) {
        let made = (table.version(), table.retagged);
        if self.made == Some(made) {
            return;
        }
        self.made = Some(made);
        let (cells, tags) = (table.column(column), table.tags());
        let mut order: Vec<(u64, u32)> = (0..table.len())
            .map(|row| ((cells[row] as u64) << 32 | tags[row] as u64, row as u32))
            .collect();
        order.sort_unstable();
        self.keys.clear();
        self.rows.clear();
        self.keys.extend(order.iter().map(|entry| entry.0));
        self.rows.extend(order.iter().map(|entry| entry.1));
    }

    /// The rows holding `cell` whose tag is in `[lo, hi)`, ascending by tag
    /// and then by row. Neither bound is above `2^32`.
    #[inline]
    pub fn within(&self, cell: u32, lo: u64, hi: u64) -> &[u32] {
        let base = (cell as u64) << 32;
        let from = self.keys.partition_point(|&key| key < base + lo);
        let to = self.keys.partition_point(|&key| key < base + hi);
        &self.rows[from..to.max(from)]
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn plain(map: &[u32]) -> Dense<'_> {
        Dense {
            map,
            delta: None,
            mask: &[],
        }
    }

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
    fn a_batch_stores_the_keys_the_table_lacks_once_each() {
        let mut table = Table::new(2);
        table.insert(&[1, 2], &[], (10, 0), 0, 1);
        table.insert(&[3, 4], &[0, 5], (11, 0), 1, 1);
        let mut rows = Vec::new();
        // A key the table holds, one it holds only at a weight, a new key
        // twice, and enough more for the index to grow under the batch.
        let mut keys = [vec![1, 3, 7, 7], vec![2, 4, 8, 8]];
        for cell in 20..60 {
            keys[0].push(cell);
            keys[1].push(cell);
        }
        let values: Vec<u32> = (100..100 + keys[0].len() as u32).collect();
        table.probe_or_insert(
            &[&keys[0], &keys[1]],
            &[],
            Values::Each(&values),
            50,
            2,
            &mut rows,
        );
        assert_eq!(rows[..4], [0, u32::MAX, u32::MAX, 3]);
        assert!(rows[4..].iter().all(|&row| row == u32::MAX));
        assert_eq!(table.len(), 2 + 2 + 40);
        assert_eq!(table.get(&[3, 4], &[]), Some(2));
        assert_eq!(table.get(&[3, 4], &[0, 5]), Some(1));
        assert_eq!(table.get(&[7, 8], &[]), Some(3));
        assert_eq!(table.get(&[59, 59], &[]), Some(43));
        assert_eq!((table.values()[3], table.tags()[3]), (102, 51));
        assert_eq!((table.values()[43], table.tags()[43]), (143, 91));
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
        assert!(table.repair(&plain(&map), None, 2, false, &mut report));
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
        let shift = Dense {
            map: &map,
            delta: Some(&delta),
            mask: &mask,
        };
        let mut report = Repair::default();
        table.repair(&shift, None, 2, false, &mut report);
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
    fn repair_keeps_an_unordered_pair_in_order() {
        let mut table = Table::new(3).plain(1).unordered([1, 2]);
        table.insert(&[9, 1, 5], &[], (4, 0), 0, 1);
        table.insert(&[9, 2, 3], &[0, 0, 4], (4, 0), 1, 1);
        table.insert(&[9, 0, 5], &[], (4, 0), 2, 1);
        // 5 is 0, and 3 is 2 + 1 modulo 2^8: the first row reads (0, 1) and
        // meets no other, the second (2, 2 + 5) in order, the third (0, 0).
        let map = [0, 1, 2, 2, 4, 0];
        let delta = [0, 0, 0, 1, 0, 0];
        let mask = [0xff; 6];
        let shift = Dense {
            map: &map,
            delta: Some(&delta),
            mask: &mask,
        };
        let mut report = Repair::default();
        table.repair(&shift, None, 2, false, &mut report);
        assert!(report.collisions.is_empty());
        assert_eq!(table.get(&[9, 0, 1], &[]), Some(0));
        assert_eq!(table.get(&[9, 1, 0], &[]), None);
        assert_eq!(table.get(&[9, 2, 2], &[0, 0, 5]), Some(1));
        assert_eq!(table.get(&[9, 0, 0], &[]), Some(2));
        // A weight alone orders two cells that became one.
        let mut table = Table::new(3).plain(1).unordered([1, 2]);
        table.insert(&[9, 1, 2], &[0, 7, 0], (4, 0), 0, 1);
        let map = [0, 1, 1, 3, 4];
        let delta = [0, 0, 3, 0, 0];
        let shift = Dense {
            map: &map,
            delta: Some(&delta),
            mask: &mask,
        };
        table.repair(&shift, None, 2, false, &mut report);
        assert_eq!(table.get(&[9, 1, 1], &[0, 3, 7]), Some(0));
    }

    #[test]
    fn a_bag_keeps_rows_that_come_to_share_a_key() {
        let mut table = Table::bag(1);
        table.insert(&[0], &[], (10, 0), 0, 1);
        table.insert(&[1], &[], (11, 0), 1, 1);
        let mut report = Repair::default();
        assert!(table.repair(
            &plain(&[0, 0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
            None,
            2,
            false,
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
            table.repair(&plain(&map), None, 2, false, &mut report);
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

        /// However the table grew and was repaired between refreshes, with a
        /// repair dissolving some of the rows it re-keyed or not, a grouping
        /// lists each cell's rows ascending.
        #[test]
        fn a_group_lists_each_cells_rows_ascending(
            rows in prop::collection::vec((0u32..32, 0u32..32, 0u32..32), 0..300),
            cuts in prop::collection::vec((0usize..300, prop::option::of(1u32..32)), 0..8),
            keyed in any::<bool>(),
            dissolving in any::<bool>(),
            column in 0usize..3,
        ) {
            let mut table = if keyed { Table::new(2) } else { Table::bag(2) };
            let mut dissolve = |row: RowView<'_>| row.value.0.is_multiple_of(3);
            let mut group = Group::default();
            let mut cuts: Vec<_> = cuts.into_iter().map(|(cut, repair)| (cut.min(rows.len()), repair)).collect();
            cuts.sort();
            cuts.push((rows.len(), None));
            let mut inserted = 0;
            for (cut, moved) in cuts {
                for &(a, b, value) in &rows[inserted..cut] {
                    if !keyed || table.get(&[a, b], &[]).is_none() {
                        table.insert(&[a, b], &[], (value, 0), 0, 1);
                    }
                }
                inserted = cut;
                // One cell comes to stand for the one below it.
                if let Some(moved) = moved {
                    let map: Vec<u32> = (0..32).map(|cell| if cell == moved { moved - 1 } else { cell }).collect();
                    let dissolve = dissolving
                        .then_some(&mut dissolve as &mut dyn FnMut(RowView<'_>) -> bool);
                    table.repair(&plain(&map), dissolve, 2, false, &mut Repair::default());
                }
                group.refresh(&table, column, 32);
                let cells = table.column(column);
                for cell in 0..34 {
                    let expected: Vec<u32> = (0..table.len() as u32).filter(|&row| cells[row as usize] == cell).collect();
                    prop_assert_eq!(group.rows(cell), &expected[..]);
                }
            }
        }
    }
}
