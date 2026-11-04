use std::cell::RefCell;
use std::hint::select_unpredictable;
use std::ops::Range;

use fearless_simd::{Level, Simd, dispatch, prelude::*};

use crate::CacheInfo;

#[cfg(kani)]
mod proofs;

// The longest range narrowed by compares, as each cell of it takes one bit of
// a `u64` mask; longer ones are searched. Columns are padded by as many cells.
const WINDOW: usize = 64;
// The longest bucket a build ranks by compares, one bit per key of a `u64`
// mask; longer ones are sorted.
const KEYS: usize = 64;

/// Rows of `u32` columns sorted by column order, for prefix lookups.
///
/// Level 1 is an offsets array indexed by the first value less the smallest,
/// so a lookup on the first column is two loads with no hashing or search.
/// That suits columns of dense ids, such as e-class ids handed out in
/// increasing order. When the first values are spread too thin for that, the
/// array is indexed by the position of the value among the distinct first
/// values instead, found by binary search, so that building and memory stay
/// proportional to the rows. Each deeper level searches the range of the
/// level above it.
#[derive(Clone)]
pub struct DenseTrie {
    // Rows whose first cell is `v` are at `offsets[i]..offsets[i + 1]`, with
    // `i` either `v - base` or, when `values` holds the distinct first values,
    // the position of `v` in it. The last two entries both hold the row
    // count, so a value outside can be clamped to an empty range instead of
    // branching.
    offsets: Vec<u32>,
    base: u32,
    values: Vec<u32>,
    // Each column is followed by `WINDOW` padding cells, so block loads that
    // start at any row stay in bounds.
    columns: Vec<Vec<u32>>,
    rows: usize,
    // Kept for the next rebuild: which rows were live, the rows packed into
    // keys while they are sorted, and counts of a second column.
    live: Vec<u64>,
    keys: Vec<u64>,
    counts: Vec<u32>,
}

impl DenseTrie {
    /// Copy the rows of `columns` for which `live(row)` holds, sorted by
    /// `(columns[0], columns[1], ...)`.
    ///
    /// # Panics
    ///
    /// If `columns` is empty or its columns differ in length.
    pub fn build(columns: &[&[u32]], live: impl Fn(usize) -> bool) -> Self {
        let mut trie = Self {
            offsets: Vec::new(),
            base: 0,
            values: Vec::new(),
            columns: Vec::new(),
            rows: 0,
            live: Vec::new(),
            keys: Vec::new(),
            counts: Vec::new(),
        };
        trie.rebuild(columns, live);
        trie
    }

    /// [`DenseTrie::build`] in place, reusing this trie's memory, so a
    /// rebuild after the table changed does not fault in fresh pages.
    ///
    /// # Panics
    ///
    /// If `columns` is empty or its columns differ in length.
    pub fn rebuild(&mut self, columns: &[&[u32]], live: impl Fn(usize) -> bool) {
        let (first, rest) = columns.split_first().expect("a trie has a first column");
        assert!(
            rest.iter().all(|column| column.len() == first.len()),
            "columns of one table"
        );
        let in_order = rest.last().is_some_and(|column| ascending(column));
        let (base, span) = bounds(first);
        if !counted(span, first.len()) {
            return self.rebuild_sorted(first, rest, in_order, live);
        }
        (self.base, self.values) = (base, Vec::new());
        // A counting sort keeps table order inside a bucket, and the last
        // column is often in table order, as row ids are. Then sorting by the
        // middle column first, by counting while its counters stay in L2,
        // leaves no bucket to sort after the first column.
        let middle = match rest {
            [middle, _] if in_order => Some((*middle, bounds(middle))),
            _ => None,
        };
        let l2 = CacheInfo::detect().l2;
        let middle = middle.filter(|&(_, (_, span))| {
            counted(span, first.len()) && l2.is_some_and(|l2| 4 * span <= l2.get())
        });
        let total = self.count(first, middle, live);
        let (rows, width) = (total as usize, self.offsets.len() - 2);
        let middle_base = middle.map_or(0, |(_, (base, _))| base);
        let middle = middle.map(|(middle, _)| middle);
        // Every row of a column is written below, so only the padding of a
        // longer column needs a value.
        self.columns.resize_with(columns.len(), Vec::new);
        for column in &mut self.columns {
            column.resize(rows + WINDOW, 0);
        }
        let (live, sorted, keys) = (&self.live, &mut self.columns, &mut self.keys);
        // Placing a row moves the cursor at `offsets[v + 1]` from the start of
        // its bucket to the end, which is where `v + 1` starts.
        let cursors = &mut self.offsets[1..];
        if let (Some(middle), [first_sorted, middle_sorted, last_sorted]) =
            (middle, &mut sorted[..])
        {
            // By the middle column into the first column and `keys`, then by
            // the first column into the other two.
            let by_middle = &mut self.counts[1..];
            keys.resize(rows + 1, 0);
            let cells = first.iter().zip(middle).zip(rest[1]);
            for (row, ((&value, &high), &low)) in cells.enumerate() {
                // SAFETY: as for `place`, and `at` is at most `rows`.
                unsafe {
                    let bit = live[row / 64] >> (row % 64) & 1;
                    let at = place(by_middle, high - middle_base, bit, total);
                    *first_sorted.get_unchecked_mut(at) = value;
                    *keys.get_unchecked_mut(at) = u64::from(high) << 32 | u64::from(low);
                }
            }
            for (&value, &key) in first_sorted[..rows].iter().zip(&keys[..rows]) {
                // SAFETY: as for `place`, and `at` is below `rows`.
                unsafe {
                    let at = place(cursors, value - base, 1, total);
                    *middle_sorted.get_unchecked_mut(at) = (key >> 32) as u32;
                    *last_sorted.get_unchecked_mut(at) = key as u32;
                }
            }
        } else if let [high, low @ ..] = rest
            && low.len() <= 1
        {
            // The other cells of a row packed into one key, so the scatter
            // moves one cell per row and a bucket sorts as plain integers.
            let (low, mask) = low.first().map_or((*high, 0), |&low| (low, u32::MAX));
            keys.resize(rows + KEYS, 0);
            let cells = first.chunks(64).zip(high.chunks(64)).zip(low.chunks(64));
            for (((first, high), low), &bits) in cells.zip(live) {
                for (bit, ((&value, &high), &low)) in first.iter().zip(high).zip(low).enumerate() {
                    // SAFETY: as for `place`, and `at` is at most `rows`.
                    unsafe {
                        let at = place(cursors, value - base, bits >> bit & 1, total);
                        *keys.get_unchecked_mut(at) = u64::from(high) << 32 | u64::from(low & mask);
                    }
                }
            }
            if !(in_order && mask == 0) {
                let buckets = &self.offsets[..=width];
                dispatch!(Level::new(), simd => sort_keys(simd, buckets, keys));
            }
            for (half, column) in [32, 0].into_iter().zip(&mut sorted[1..]) {
                for (cell, &key) in column.iter_mut().zip(&keys[..rows]) {
                    *cell = (key >> half) as u32;
                }
            }
        } else {
            let positions: Vec<usize> = first
                .iter()
                .enumerate()
                // SAFETY: as for `place`.
                .map(|(row, &value)| unsafe {
                    place(
                        cursors,
                        value - base,
                        live[row / 64] >> (row % 64) & 1,
                        total,
                    )
                })
                .collect();
            for (sorted, column) in sorted[1..].iter_mut().zip(rest) {
                for (&at, &cell) in positions.iter().zip(*column) {
                    sorted[at] = cell;
                }
            }
            if !rest.is_empty() {
                for bucket in self.offsets[..=width].windows(2) {
                    sort_rows(&mut sorted[1..], bucket[0] as usize..bucket[1] as usize);
                }
            }
        }
        // The first cell of row `r` is one less than the number of buckets
        // that start at or before `r`, so count bucket starts and sum them.
        let column = &mut sorted[0][..=rows];
        column.fill(0);
        for &start in &self.offsets[..width] {
            column[start as usize] += 1;
        }
        let mut starts = 0u32;
        for cell in &mut column[..rows] {
            starts += *cell;
            *cell = base + (starts - 1);
        }
        self.rows = rows;
    }

    // `rebuild` for first values too sparse to count: the live rows sorted by
    // first value and then row, the distinct first values and their offsets
    // read off them, and each bucket sorted by the other columns.
    fn rebuild_sorted(
        &mut self,
        first: &[u32],
        rest: &[&[u32]],
        in_order: bool,
        live: impl Fn(usize) -> bool,
    ) {
        let order = &mut self.keys;
        order.clear();
        order.extend(
            (0..first.len())
                .filter(|&row| live(row))
                .map(|row| u64::from(first[row]) << 32 | row as u64),
        );
        order.sort_unstable();
        let rows = order.len();
        self.base = 0;
        self.values.clear();
        self.offsets.clear();
        for (at, &key) in order.iter().enumerate() {
            let value = (key >> 32) as u32;
            if self.values.last() != Some(&value) {
                self.values.push(value);
                self.offsets.push(at as u32);
            }
        }
        self.offsets.extend([rows as u32; 2]);
        self.columns.resize_with(rest.len() + 1, Vec::new);
        for column in &mut self.columns {
            column.resize(rows + WINDOW, 0);
        }
        let (sorted, buckets) = (&mut self.columns, &self.offsets[..=self.values.len()]);
        if let [high, low @ ..] = rest
            && low.len() <= 1
        {
            // Each row's key replaces its place in `order`, as in `rebuild`.
            let (low, mask) = low.first().map_or((*high, 0), |&low| (low, u32::MAX));
            order.resize(rows + KEYS, 0);
            for (key, cell) in order[..rows].iter_mut().zip(&mut sorted[0][..]) {
                let row = *key as u32 as usize;
                *cell = (*key >> 32) as u32;
                *key = u64::from(high[row]) << 32 | u64::from(low[row] & mask);
            }
            if !(in_order && mask == 0) {
                dispatch!(Level::new(), simd => sort_keys(simd, buckets, order));
            }
            for (half, column) in [32, 0].into_iter().zip(&mut sorted[1..]) {
                for (cell, &key) in column.iter_mut().zip(&order[..rows]) {
                    *cell = (key >> half) as u32;
                }
            }
        } else {
            for (at, &key) in order.iter().enumerate() {
                sorted[0][at] = (key >> 32) as u32;
                for (sorted, column) in sorted[1..].iter_mut().zip(rest) {
                    sorted[at] = column[key as u32 as usize];
                }
            }
            if !rest.is_empty() {
                for bucket in buckets.windows(2) {
                    sort_rows(&mut sorted[1..], bucket[0] as usize..bucket[1] as usize);
                }
            }
        }
        self.rows = rows;
    }

    // In one pass over the rows: `live` as bits, the counts of `first` in
    // `offsets` and those of `middle`, of the given width, in `counts`, each
    // summed so `counts[v + 1]` is where the rows of `v` start. Returns the
    // number of live rows.
    fn count(
        &mut self,
        first: &[u32],
        middle: Option<(&[u32], (u32, usize))>,
        live: impl Fn(usize) -> bool,
    ) -> u32 {
        let (base, span) = bounds(first);
        self.offsets.clear();
        self.offsets.resize(span + 2, 0);
        self.counts.clear();
        self.counts
            .resize(middle.map_or(0, |(_, (_, span))| span) + 2, 0);
        let middle_base = middle.map_or(0, |(_, (base, _))| base);
        let middle = middle.map(|(middle, _)| middle);
        self.live.clear();
        for (word, cells) in first.chunks(64).enumerate() {
            let mut bits = 0;
            for (bit, &value) in cells.iter().enumerate() {
                let row = 64 * word + bit;
                let live = live(row);
                bits |= u64::from(live) << bit;
                // SAFETY: each array is two longer than the largest value
                // it counts, and `middle` is as long as `first`.
                unsafe {
                    *self.offsets.get_unchecked_mut((value - base) as usize + 2) += u32::from(live);
                    if let Some(middle) = middle {
                        let value = *middle.get_unchecked(row) - middle_base;
                        *self.counts.get_unchecked_mut(value as usize + 2) += u32::from(live);
                    }
                }
            }
            self.live.push(bits);
        }
        for counts in [&mut self.offsets, &mut self.counts] {
            let mut total = 0;
            for count in counts.iter_mut() {
                total += *count;
                *count = total;
            }
        }
        self.offsets[self.offsets.len() - 1]
    }

    /// [`DenseTrie::rebuild`] after the table grew by the rows from `appended`
    /// on and some of its rows died, merging the new rows in instead of
    /// sorting every row again when they are few. Each row's last cell must be its row id, its
    /// index in `columns`, as the rows already in the trie are checked
    /// against `live` by it.
    ///
    /// # Panics
    ///
    /// If `columns` is not as many columns as the trie's, of one length at
    /// least `appended`, or an appended row's last cell is not its row id.
    pub fn merge(&mut self, columns: &[&[u32]], appended: usize, live: impl Fn(usize) -> bool) {
        assert_eq!(columns.len(), self.columns.len(), "the trie's columns");
        let ids = columns.last().expect("a trie has a first column");
        assert!(
            ids[appended..]
                .iter()
                .zip(appended..)
                .all(|(&id, row)| id as usize == row),
            "row ids in the last column"
        );
        // Merging moves every old row once more, which pays only while the
        // new rows are few: past an eighth of the old ones, the egglog
        // benches ran faster rebuilding.
        if columns[0].len() - appended > appended / 8 {
            return self.rebuild(columns, live);
        }
        let added: Vec<&[u32]> = columns.iter().map(|column| &column[appended..]).collect();
        let added = Self::build(&added, |row| live(appended + row));
        let kept = dispatch!(Level::new(), simd => self.drop_dead(simd, &live));
        // From the back, each new row goes after the old rows not above it,
        // and the old rows past it move up by the new rows still to place.
        let rows = kept + added.rows;
        for column in &mut self.columns {
            column.resize(rows + WINDOW, 0);
        }
        let mut end = kept;
        for new in (0..added.rows).rev() {
            let (mut at, mut above) = (0, end);
            while at < above {
                let old = (at + above) / 2;
                if key(&self.columns, old).le(key(&added.columns, new)) {
                    at = old + 1;
                } else {
                    above = old;
                }
            }
            for (column, added) in self.columns.iter_mut().zip(&added.columns) {
                column.copy_within(at..end, at + new + 1);
                column[at + new] = added[new];
            }
            end = at;
        }
        for column in &mut self.columns {
            column[rows..].fill(0);
        }
        self.rows = rows;
        self.index_first();
    }

    // Keep only the rows whose row id, the last cell, is live, in order, and
    // return how many there are. A vector of rows at a time: the live ones
    // are packed to the front of each column's vector and stored where the
    // kept rows end, which is never past the vector being read.
    #[inline(always)]
    fn drop_dead<S: Simd>(&mut self, simd: S, live: impl Fn(usize) -> bool) -> usize {
        let lanes = S::u32s::LEN;
        let (rows, ids) = (self.rows, self.columns.len() - 1);
        let mut kept = 0;
        for start in (0..rows).step_by(lanes) {
            let block = start..start + lanes;
            let bits = (0..lanes.min(rows - start)).fold(0, |bits, lane| {
                bits | u64::from(live(self.columns[ids][start + lane] as usize)) << lane
            });
            let keep = S::mask32s::from_bitmask(simd, bits);
            for column in &mut self.columns {
                let cells = S::u32s::from_slice(simd, &column[block.clone()]);
                cells
                    .compress(keep)
                    .store_slice(&mut column[kept..kept + lanes]);
            }
            kept += bits.count_ones() as usize;
        }
        kept
    }

    // Level 1 from the first column, sorted.
    fn index_first(&mut self) {
        let (first, rows) = (&self.columns[0][..self.rows], self.rows as u32);
        let (base, span) = bounds(first);
        self.offsets.clear();
        self.values.clear();
        if counted(span, first.len()) {
            self.base = base;
            self.offsets.resize(span + 2, 0);
            for &value in first {
                self.offsets[(value - base) as usize + 1] += 1;
            }
            let mut total = 0;
            for offset in &mut self.offsets {
                total += *offset;
                *offset = total;
            }
        } else {
            self.base = 0;
            for (at, &value) in first.iter().enumerate() {
                if self.values.last() != Some(&value) {
                    self.values.push(value);
                    self.offsets.push(at as u32);
                }
            }
            self.offsets.extend([rows; 2]);
        }
    }

    /// The positions of the rows that start with each prefix of a batch
    /// given as columns: prefix `i` is `(prefixes[0][i], prefixes[1][i], ...)`, and its range
    /// goes to `out[i]`.
    ///
    /// # Panics
    ///
    /// If a prefix column is not `out.len()` long.
    pub fn ranges(&self, prefixes: &[&[u32]], out: &mut [Range<u32>]) {
        assert!(
            prefixes.iter().all(|column| column.len() == out.len()),
            "one cell per prefix in each column"
        );
        match prefixes {
            _ if prefixes.len() > self.columns.len() => out.fill(0..0),
            [] => out.fill(self.all()),
            [first] if self.values.is_empty() => {
                let (offsets, base) = (&self.offsets[..], self.base);
                let (inside, [_, _]) = offsets.split_at(offsets.len() - 2) else {
                    unreachable!("offsets end in two row counts")
                };
                for (range, &value) in out.iter_mut().zip(*first) {
                    let at = (value.wrapping_sub(base) as usize).min(inside.len());
                    *range = offsets[at]..offsets[at + 1];
                }
            }
            [first] => {
                for (range, &value) in out.iter_mut().zip(*first) {
                    *range = self.sparse_range(value);
                }
            }
            _ => {
                // Taken out of the thread local here, as the SIMD code must
                // not run inside a closure, where it loses its target features.
                let mut scratch = NARROWING.take();
                dispatch!(Level::new(), simd => self.ranges_with(simd, prefixes, out, &mut scratch));
                NARROWING.set(scratch);
            }
        }
    }

    // The range of first value `values[i]` as `starts[i]..ends[i]`. Kept out
    // of the SIMD context, where it would be compiled to gathers, which are
    // slower on Zen 5 than one load per key.
    #[inline(never)]
    fn first_ranges(&self, values: &[u32], starts: &mut [u32], ends: &mut [u32]) {
        let ranges = starts.iter_mut().zip(ends).zip(values);
        if self.values.is_empty() {
            for ((start, end), &value) in ranges {
                Range {
                    start: *start,
                    end: *end,
                } = self.dense_range(value);
            }
        } else {
            for ((start, end), &value) in ranges {
                Range {
                    start: *start,
                    end: *end,
                } = self.sparse_range(value);
            }
        }
    }

    // A chunk of prefixes at a time, level by level, so the loads of one level
    // do not wait on each other and the ranges stay in L1 between levels.
    #[inline(always)]
    fn ranges_with<S: Simd>(
        &self,
        simd: S,
        prefixes: &[&[u32]],
        out: &mut [Range<u32>],
        scratch: &mut Vec<u32>,
    ) {
        let chunk = chunk(S::u32s::LEN);
        // Each array a cache line further than the last from a multiple of
        // 4 KiB, so a store to one does not look to the load unit like a store
        // to the same cell of another.
        let line = CacheInfo::detect().line_size.get() / 4;
        let stride = chunk + line.max(S::u32s::LEN);
        {
            scratch.resize(scratch.len().max(5 * stride), 0);
            let mut arrays = scratch.chunks_exact_mut(stride);
            let mut next = || &mut arrays.next().expect("five arrays")[..chunk];
            let mut state = Narrowing {
                starts: next(),
                ends: next(),
                below: next(),
                through: next(),
                long: &mut arrays.next().expect("five arrays")[..],
            };
            for (at, out) in (0..).step_by(chunk).zip(out.chunks_mut(chunk)) {
                let keys = at..at + out.len();
                self.first_ranges(&prefixes[0][keys.clone()], state.starts, state.ends);
                for (cells, values) in self.columns[1..].iter().zip(&prefixes[1..]) {
                    narrow(simd, cells, &values[keys.clone()], &mut state);
                }
                for ((range, &start), &end) in out.iter_mut().zip(&*state.starts).zip(&*state.ends)
                {
                    *range = start..end;
                }
            }
        }
    }

    /// Column `i` in trie order; a range from a lookup indexes into it.
    pub fn column(&self, i: usize) -> &[u32] {
        &self.columns[i][..self.rows]
    }

    fn all(&self) -> Range<u32> {
        0..self.rows as u32
    }

    // The rows whose first cell is `value`, when level 1 is indexed by value.
    #[inline(always)]
    fn dense_range(&self, value: u32) -> Range<u32> {
        let (offsets, base) = (&self.offsets[..], self.base);
        let (inside, [_, _]) = offsets.split_at(offsets.len() - 2) else {
            unreachable!("offsets end in two row counts")
        };
        let at = (value.wrapping_sub(base) as usize).min(inside.len());
        offsets[at]..offsets[at + 1]
    }

    // The rows whose first cell is `value`, when level 1 is indexed by the
    // position in `values`.
    // Without branches on the cells, so a batch of searches overlaps.
    #[inline(always)]
    fn sparse_range(&self, value: u32) -> Range<u32> {
        let (mut at, mut len) = (0, self.values.len());
        while len > 1 {
            let half = len / 2;
            at = select_unpredictable(self.values[at + half] <= value, at + half, at);
            len -= half;
        }
        let at = select_unpredictable(self.values[at] == value, at, self.values.len());
        self.offsets[at]..self.offsets[at + 1]
    }
}

thread_local! {
    // The arrays of `Narrowing`, kept so a batch does not allocate them.
    static NARROWING: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

// Prefixes looked up together: as many as keep their state, five `u32`s a
// prefix in `Narrowing`, within a quarter of L1 beside the cells the lookups
// load, rounded down to whole vectors of `lanes`. An unknown L1 is taken to
// be 32 KiB.
fn chunk(lanes: usize) -> usize {
    let l1 = CacheInfo::detect()
        .l1_data
        .map_or(32 * 1024, |size| size.get());
    (l1 / 4 / (5 * 4) / lanes).max(1) * lanes
}

// The state of a chunk of prefixes being narrowed: prefix `i` has the range
// `starts[i]..ends[i]`, `below[i]` and `through[i]` mark the cells of its
// first vector below and at most its value, and `long` lists the prefixes
// whose range is longer than a vector.
struct Narrowing<'a> {
    starts: &'a mut [u32],
    ends: &'a mut [u32],
    below: &'a mut [u32],
    through: &'a mut [u32],
    long: &'a mut [u32],
}

// Narrow each range of `state` to its cells equal to `values[i]`. Ranges are
// mostly a cell or two, so one vector compare per range replaces a search and
// its mispredicted branches; ranges longer than a vector are gathered and
// redone after the rest, so picking them out does not branch.
#[inline(always)]
fn narrow<S: Simd>(simd: S, cells: &[u32], values: &[u32], state: &mut Narrowing) {
    let lanes = S::u32s::LEN;
    let Narrowing {
        starts,
        ends,
        below,
        through,
        long,
    } = state;
    for (at, &value) in values.iter().enumerate() {
        let block = S::u32s::from_slice(simd, &cells[starts[at] as usize..][..lanes]);
        let value = S::u32s::splat(simd, value);
        below[at] = block.simd_lt(value).to_bitmask() as u32;
        through[at] = block.simd_le(value).to_bitmask() as u32;
    }
    let splat = |value: u32| S::u32s::splat(simd, value);
    let (one, width) = (splat(1), splat(lanes as u32));
    let index = S::u32s::from_fn(simd, |lane| lane as u32);
    let mut longs = 0;
    for group in (0..values.len()).step_by(lanes) {
        let at = group..group + lanes;
        let load = |cells: &[u32]| S::u32s::from_slice(simd, &cells[at.clone()]);
        let (start, end) = (load(starts), load(ends));
        let len = end - start;
        let inside = (one << len.min(width)) - one;
        let valid = (index + splat(group as u32)).simd_lt(splat(values.len() as u32));
        let is_long = len.simd_gt(width) & valid;
        let narrowed_start = start + (load(below) & inside).count_ones();
        let narrowed_end = start + (load(through) & inside).count_ones();
        is_long
            .select(start, narrowed_start)
            .store_slice(&mut starts[at.clone()]);
        is_long
            .select(end, narrowed_end)
            .store_slice(&mut ends[at.clone()]);
        let indices = index + splat(group as u32);
        indices
            .compress(is_long)
            .store_slice(&mut long[longs..longs + lanes]);
        longs += is_long.to_bitmask().count_ones() as usize;
    }
    for &at in &long[..longs] {
        let at = at as usize;
        let (start, end) = (starts[at], ends[at]);
        let range = if end - start > WINDOW as u32 {
            search(cells, start..end, values[at])
        } else {
            let range = window(
                simd,
                &cells[start as usize..][..WINDOW],
                end - start,
                values[at],
            );
            start + range.start..start + range.end
        };
        (starts[at], ends[at]) = (range.start, range.end);
    }
}

// The cells equal to `value` among the first `len` cells of `window`, as
// positions in it.
#[inline(always)]
fn window<S: Simd>(simd: S, window: &[u32], len: u32, value: u32) -> Range<u32> {
    let lanes = S::u32s::LEN;
    let value = S::u32s::splat(simd, value);
    let (mut below, mut through) = (0, 0);
    for (shift, block) in (0..).step_by(lanes).zip(window.chunks_exact(lanes)) {
        let block = S::u32s::from_slice(simd, block);
        below |= block.simd_lt(value).to_bitmask() << shift;
        through |= block.simd_le(value).to_bitmask() << shift;
    }
    let inside = u64::MAX >> (WINDOW as u32 - len);
    (below & inside).count_ones()..(through & inside).count_ones()
}

fn search(cells: &[u32], range: Range<u32>, value: u32) -> Range<u32> {
    let cells = &cells[range.start as usize..range.end as usize];
    let low = cells.partition_point(|&cell| cell < value);
    let high = low + cells[low..].partition_point(|&cell| cell == value);
    range.start + low as u32..range.start + high as u32
}

// The smallest cell, and how many values lie from it to the largest.
fn bounds(cells: &[u32]) -> (u32, usize) {
    let (low, high) = cells.iter().fold((u32::MAX, 0), |(low, high), &cell| {
        (low.min(cell), high.max(cell))
    });
    if cells.is_empty() {
        (0, 0)
    } else {
        (low, (high - low) as usize + 1)
    }
}

// Whether counting over a window of `span` values beats sorting `rows` rows:
// counting costs a counter per value of the window, sorting about log2(rows)
// compares per row.
fn counted(span: usize, rows: usize) -> bool {
    span <= rows * (usize::BITS - rows.leading_zeros()) as usize
}

// Whether `column` never decreases, checked without branches so it runs at
// vector speed.
fn ascending(column: &[u32]) -> bool {
    let pairs = column.iter().zip(column.iter().skip(1));
    pairs.fold(true, |ascending, (a, b)| ascending & (a <= b))
}

// Where a row of first value `value` goes, moving its cursor past it; a dead
// row goes to `sink`, the live row count, so every slot handed out is at most
// `sink`.
//
// # Safety
//
// `value` is below `cursors.len()`, as it is for cursors made from counts of
// the column `value` comes from.
#[inline(always)]
unsafe fn place(cursors: &mut [u32], value: u32, live: u64, sink: u32) -> usize {
    // SAFETY: the caller keeps `value` in bounds.
    let cursor = unsafe { cursors.get_unchecked_mut(value as usize) };
    let at = if live == 1 { *cursor } else { sink };
    *cursor += live as u32;
    at as usize
}

// Sort each bucket of `keys`. Most buckets hold one or two keys, so a pair is
// ordered without branching, and longer ones are ranked by block compares
// instead of a sort's mispredicted branches.
#[inline(always)]
fn sort_keys<S: Simd>(simd: S, buckets: &[u32], keys: &mut [u64]) {
    let width = S::u64s::LEN;
    let lanes = S::u64s::from_fn(simd, |lane| lane as u64);
    let mut ranked = [0; KEYS];
    for bucket in buckets.windows(2) {
        let (start, len) = (bucket[0] as usize, (bucket[1] - bucket[0]) as usize);
        if len <= 2 {
            let (a, b) = (keys[start], keys[start + 1]);
            let swap = len == 2 && b < a;
            keys[start] = if swap { b } else { a };
            keys[start + 1] = if swap { a } else { b };
        } else if len <= KEYS {
            // A key's rank is how many keys of its bucket come before it.
            let blocks = &keys[start..start + KEYS];
            let inside = u64::MAX >> (KEYS - len);
            for (at, &cell) in blocks[..len].iter().enumerate() {
                let key = S::u64s::splat(simd, cell);
                let at = S::u64s::splat(simd, at as u64);
                let mut first = 0;
                for (lane, block) in (0..len).step_by(width).zip(blocks.chunks_exact(width)) {
                    let block = S::u64s::from_slice(simd, block);
                    let before = (lanes + S::u64s::splat(simd, lane as u64)).simd_lt(at);
                    let bits = block.simd_lt(key) | (block.simd_eq(key) & before);
                    first |= bits.to_bitmask() << lane;
                }
                ranked[(first & inside).count_ones() as usize] = cell;
            }
            keys[start..start + len].copy_from_slice(&ranked[..len]);
        } else {
            keys[start..start + len].sort_unstable();
        }
    }
}

// Sort the rows at `bucket` by `columns`, which are all but the first column.
fn sort_rows(columns: &mut [Vec<u32>], bucket: Range<usize>) {
    if bucket.len() < 2 {
        return;
    }
    let mut order: Vec<usize> = bucket.clone().collect();
    order.sort_unstable_by(|&a, &b| key(columns, a).cmp(key(columns, b)));
    for column in columns {
        let cells: Vec<u32> = order.iter().map(|&row| column[row]).collect();
        column[bucket.clone()].copy_from_slice(&cells);
    }
}

fn key(columns: &[Vec<u32>], row: usize) -> impl Iterator<Item = u32> {
    columns.iter().map(move |column| column[row])
}
