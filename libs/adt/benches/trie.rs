use std::hint::black_box;
use std::ops::Range;

use fearless_simd::{Level, Simd, dispatch, prelude::*, u32x16};

use tir_adt::DenseTrie;
use tir_bench::{BatchSize, Criterion, TMAMetrics, Throughput, criterion_group, criterion_main};

const SEED: u64 = 0x6375_636b_6f6f_0001;

fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

const BATCH: usize = 1024;
// Batches cycle through this many keys, so keys repeat rarely and come from
// memory the way the engine's columns do.
const POOL: usize = 64 * BATCH;

// An e-graph table such as `Add(a, b) -> out`, as the egglog engine indexes it
// on `(a, b)`: the trie columns are `a`, `b` and the row id. About 40% of the
// first-column ids have no rows, most of the others have one or two, and a few
// have up to 64. `b` ranges over twice the row count, as e-class ids do.
struct Table {
    columns: [Vec<u32>; 3],
    live: Vec<u64>,
    // Every first-column id is below this.
    universe: u32,
}

impl Table {
    fn new(rows: usize, seed: u64) -> Self {
        let mut state = SEED ^ seed << 32 ^ rows as u64;
        let mut first = Vec::with_capacity(rows);
        let mut universe = 0;
        while first.len() < rows {
            let id = universe;
            universe += 1;
            if random(&mut state) % 10 < 4 {
                continue;
            }
            let fanout = match random(&mut state) % 10 {
                0..6 => 1,
                6..9 => 2,
                _ => (3 + random(&mut state) % (2 << (random(&mut state) % 6))).min(64),
            };
            first.extend(std::iter::repeat_n(
                id,
                (fanout as usize).min(rows - first.len()),
            ));
        }
        for i in (1..rows).rev() {
            first.swap(i, random(&mut state) as usize % (i + 1));
        }
        let second = (0..rows)
            .map(|_| (random(&mut state) % (2 * rows as u64)) as u32)
            .collect();
        // Unions and subsumption retire about one row in 64.
        let live = (0..rows.div_ceil(64))
            .map(|_| !(1 << (random(&mut state) % 64)))
            .collect();
        Self {
            columns: [first, second, (0..rows as u32).collect()],
            live,
            universe,
        }
    }

    // A small table of an engine with many tables over one e-class id
    // space: its first and second cells are ids drawn from all `ids`.
    fn spread(rows: usize, ids: u32, seed: u64) -> Self {
        let mut state = SEED ^ seed << 32 ^ rows as u64;
        let mut id = || (random(&mut state) % u64::from(ids)) as u32;
        let first = (0..rows).map(|_| id()).collect();
        let second = (0..rows).map(|_| id()).collect();
        let live = (0..rows.div_ceil(64))
            .map(|word| !(1 << (word % 64)))
            .collect();
        Self {
            columns: [first, second, (0..rows as u32).collect()],
            live,
            universe: ids,
        }
    }

    fn live(&self, row: usize) -> bool {
        self.live[row / 64] >> (row % 64) & 1 != 0
    }

    fn build(&self) -> DenseTrie {
        let columns = self.columns.each_ref().map(Vec::as_slice);
        DenseTrie::build(&columns, |row| self.live(row))
    }

    // First-column keys, a sixteenth of them past the largest id; the second
    // column of half the keys comes from a row, so about half the two-column
    // keys are present.
    fn probes(&self, second_span: u64) -> [Vec<u32>; 2] {
        let mut state = SEED ^ u64::from(self.universe);
        let rows = self.columns[0].len() as u64;
        let span = u64::from(self.universe + self.universe / 16);
        let mut keys = [Vec::with_capacity(POOL), Vec::with_capacity(POOL)];
        for _ in 0..POOL {
            let (first, second) = if random(&mut state).is_multiple_of(2) {
                let row = (random(&mut state) % rows) as usize;
                (self.columns[0][row], self.columns[1][row])
            } else {
                let first = (random(&mut state) % span) as u32;
                (first, (random(&mut state) % second_span) as u32)
            };
            keys[0].push(first);
            keys[1].push(second);
        }
        keys
    }
}

fn dense_trie(c: &mut Criterion, _tma: &TMAMetrics) {
    for rows in [1_000, 10_000, 100_000, 1_000_000] {
        // Distinct tables, so that repeated builds do not train the branch
        // predictor on one input; an engine rebuilds after the table changed.
        let tables: Vec<Table> = (0..(1 << 16) / rows + 1)
            .map(|seed| Table::new(rows, seed as u64))
            .collect();
        let table = &tables[0];

        let mut group = c.benchmark_group(format!("dense_trie/build/{rows}"));
        group.throughput(Throughput::Elements(rows as u64));
        let mut next = tables.iter().cycle();
        group.bench_function("trie", |b| {
            b.iter(|| black_box(next.next().unwrap().build()))
        });
        let mut trie = table.build();
        group.bench_function("rebuild", |b| {
            b.iter(|| {
                let table = next.next().unwrap();
                let columns = table.columns.each_ref().map(Vec::as_slice);
                trie.rebuild(&columns, |row| table.live(row));
                black_box(&trie);
            })
        });
        group.bench_function("bound_copy", |b| {
            b.iter(|| black_box(table.columns.each_ref().map(|column| column.to_vec())))
        });
        group.bench_function("bound_counting_sort", |b| {
            b.iter(|| black_box(counting_sort(&table.columns, table.universe as usize)))
        });
        // The cheapest patterns found that give a trie's order, (first,
        // second, row id), on buffers kept from the last call: counting sorts
        // by the second column and then the first, or a counting sort by the
        // first and then each bucket sorted.
        let mut got = Sorted::default();
        let trie = table.build();
        for bound in [Sorted::radix, Sorted::buckets] {
            let rows = bound(&mut got, table);
            assert!((0..3).all(|c| trie.column(c) == &got.columns[c][..rows]));
        }
        let mut sorted = Sorted::default();
        group.bench_function("bound_radix", |b| {
            b.iter(|| sorted.radix(black_box(next.next().unwrap())))
        });
        group.bench_function("bound_buckets", |b| {
            b.iter(|| sorted.buckets(black_box(next.next().unwrap())))
        });
        group.finish();

        // An engine's refresh after it appended a hundredth of the rows and
        // killed some old ones: merged in, or the whole table rebuilt.
        let appended = rows - rows / 100;
        let columns = table.columns.each_ref().map(Vec::as_slice);
        let before: Vec<&[u32]> = columns.iter().map(|column| &column[..appended]).collect();
        let mut group = c.benchmark_group(format!("dense_trie/refresh/{rows}"));
        group.throughput(Throughput::Elements(rows as u64));
        // Both start from the trie before the refresh, built outside the
        // timed part.
        let fresh = || DenseTrie::build(&before, |_| true);
        group.bench_function("merge", |b| {
            b.iter_batched(
                fresh,
                |mut trie| trie.merge(&columns, appended, |row| table.live(row)),
                BatchSize::LargeInput,
            )
        });
        group.bench_function("rebuild", |b| {
            b.iter_batched(
                fresh,
                |mut trie| trie.rebuild(&columns, |row| table.live(row)),
                BatchSize::LargeInput,
            )
        });
        group.finish();

        let trie = table.build();
        let probes = table.probes(2 * rows as u64);
        for width in [1, 2] {
            let keys: Vec<&[u32]> = probes[..width].iter().map(Vec::as_slice).collect();
            let mut ranges = vec![0..0; POOL];

            let mut group = c.benchmark_group(format!("dense_trie/ranges{width}/{rows}"));
            group.throughput(Throughput::Elements(BATCH as u64));
            let mut batches = (0..POOL).step_by(BATCH).cycle();
            group.bench_function("batch", |b| {
                b.iter(|| {
                    let at = batches.next().unwrap();
                    let batch: Vec<&[u32]> =
                        keys.iter().map(|column| &column[at..at + BATCH]).collect();
                    trie.ranges(black_box(&batch), &mut ranges[..BATCH]);
                    black_box(&ranges);
                })
            });
            // The bare memory pattern of a lookup: a pair of adjacent cells
            // from an array as long as the offsets, then for two columns a
            // cell of the second column at the first of them.
            let span = keys[0].iter().max().unwrap() + 2;
            let offsets: Vec<u32> = (0..span).map(|id| id % rows as u32).collect();
            let column = &table.columns[1];
            let mut pairs = vec![0u64; BATCH];
            group.bench_function("bound_loads", |b| {
                b.iter(|| {
                    let at = batches.next().unwrap();
                    let batch = &keys[0][at..at + BATCH];
                    // Eight keys per iteration, so loop overhead does not
                    // hide the loads and stores.
                    let lanes = pairs
                        .as_chunks_mut::<8>()
                        .0
                        .iter_mut()
                        .zip(batch.as_chunks::<8>().0);
                    let (offsets, column) = (offsets.as_ptr(), column.as_ptr());
                    // SAFETY: keys are below `span - 1`, and offsets below `rows`.
                    unsafe {
                        if width == 1 {
                            for (pairs, keys) in lanes {
                                for (pair, &key) in pairs.iter_mut().zip(keys) {
                                    let pair_at = offsets.add(key as usize);
                                    *pair = pair_at.cast::<u64>().read_unaligned();
                                }
                            }
                        } else {
                            for (pairs, keys) in lanes {
                                for (pair, &key) in pairs.iter_mut().zip(keys) {
                                    let start = *offsets.add(key as usize);
                                    *pair = u64::from(*column.add(start as usize));
                                }
                            }
                        }
                    }
                    black_box(&pairs);
                })
            });
            let bare = Bare::new(&trie);
            let mut want = vec![0..0; BATCH];
            bare.ranges(&keys[..width], &mut want, &mut Vec::new());
            trie.ranges(
                &keys.iter().map(|c| &c[..BATCH]).collect::<Vec<_>>(),
                &mut ranges[..BATCH],
            );
            assert!(
                want.iter()
                    .zip(&ranges)
                    .all(|(a, b)| a == b || a.is_empty() && b.is_empty())
            );
            let mut long = Vec::with_capacity(BATCH);
            group.bench_function("bound_kernel", |b| {
                b.iter(|| {
                    let at = batches.next().unwrap();
                    let batch = [&keys[0][at..at + BATCH], &keys[width - 1][at..at + BATCH]];
                    bare.ranges(black_box(&batch[..width]), &mut want, &mut long);
                    black_box(&want);
                })
            });
            group.finish();
        }
    }
}

// The memory pattern of a build with nothing else: count the first column,
// sum the counts, and scatter the other two cells of each row as one 8-byte
// payload through the running cursors.
fn counting_sort(columns: &[Vec<u32>; 3], universe: usize) -> (Vec<u32>, Vec<u64>) {
    let mut offsets = vec![0u32; universe + 1];
    let mut sorted = vec![0u64; columns[0].len()];
    // SAFETY: first cells are below `universe`, and every cursor stays below
    // the row count.
    unsafe {
        for &value in &columns[0] {
            *offsets.get_unchecked_mut(value as usize + 1) += 1;
        }
        for at in 1..offsets.len() {
            *offsets.get_unchecked_mut(at) += *offsets.get_unchecked(at - 1);
        }
        for row in 0..columns[0].len() {
            let cursor = offsets.get_unchecked_mut(*columns[0].get_unchecked(row) as usize);
            let payload = u64::from(*columns[1].get_unchecked(row)) << 32
                | u64::from(*columns[2].get_unchecked(row));
            *sorted.get_unchecked_mut(*cursor as usize) = payload;
            *cursor += 1;
        }
    }
    (offsets, sorted)
}

// The rows of a table in trie order: `offsets` as in `DenseTrie`, the three
// columns sorted, and scratch kept between calls. Indexing is unchecked, as a
// bound should not pay for bounds checks; every index is a first or second
// cell, below its counters' length, or a cursor, below the live row count.
#[derive(Default)]
struct Sorted {
    offsets: Vec<u32>,
    columns: [Vec<u32>; 3],
    counts: Vec<u32>,
    firsts: Vec<u32>,
    keys: Vec<u64>,
}

impl Sorted {
    fn resize(&mut self, table: &Table) {
        let rows = table.columns[0].len();
        for column in &mut self.columns {
            column.resize(rows + 1, 0);
        }
        self.firsts.resize(rows + 1, 0);
        self.keys.resize(rows + 1, 0);
    }

    // `counts[v + 1]` is where the live rows of `v` start; returns their number.
    fn count(counts: &mut Vec<u32>, cells: &[u32], table: &Table) -> usize {
        let width = *cells.iter().max().unwrap() as usize + 1;
        counts.clear();
        counts.resize(width + 2, 0);
        let counters = counts.as_mut_ptr();
        for (row, &cell) in cells.iter().enumerate() {
            // SAFETY: `cell + 2 < width + 2`.
            unsafe { *counters.add(cell as usize + 2) += u32::from(table.live(row)) };
        }
        let mut sum = 0;
        for count in counts.iter_mut() {
            sum += *count;
            *count = sum;
        }
        sum as usize
    }

    // The first column from the offsets, as `DenseTrie` fills it.
    fn finish(&mut self, rows: usize) {
        let first = &mut self.columns[0];
        first[..=rows].fill(0);
        let width = self.offsets.len() - 2;
        for &start in &self.offsets[..width] {
            first[start as usize] += 1;
        }
        let mut starts = 0;
        for cell in &mut first[..rows] {
            starts += *cell;
            *cell = starts - 1;
        }
    }

    fn radix(&mut self, table: &Table) -> usize {
        self.resize(table);
        let [first, second, ids] = &table.columns;
        let rows = Self::count(&mut self.offsets, first, table);
        Self::count(&mut self.counts, second, table);
        let (by_second, by_first) = (self.counts.as_mut_ptr(), self.offsets.as_mut_ptr());
        let (firsts, keys) = (self.firsts.as_mut_ptr(), self.keys.as_mut_ptr());
        let [_, high, low] = &mut self.columns;
        let (high, low) = (high.as_mut_ptr(), low.as_mut_ptr());
        // SAFETY: see `Sorted`; dead rows go to the slot at `rows`.
        unsafe {
            for (row, (&value, &cell)) in first.iter().zip(second).enumerate() {
                let live = u32::from(table.live(row));
                let cursor = by_second.add(cell as usize + 1);
                let at = if live == 1 { *cursor } else { rows as u32 } as usize;
                *cursor += live;
                *firsts.add(at) = value;
                *keys.add(at) = u64::from(cell) << 32 | u64::from(ids[row]);
            }
            for at in 0..rows {
                let cursor = by_first.add(*firsts.add(at) as usize + 1);
                let key = *keys.add(at);
                *high.add(*cursor as usize) = (key >> 32) as u32;
                *low.add(*cursor as usize) = key as u32;
                *cursor += 1;
            }
        }
        self.finish(rows);
        rows
    }

    fn buckets(&mut self, table: &Table) -> usize {
        self.resize(table);
        let [first, second, ids] = &table.columns;
        let rows = Self::count(&mut self.offsets, first, table);
        let (by_first, keys) = (self.offsets.as_mut_ptr(), self.keys.as_mut_ptr());
        // SAFETY: see `Sorted`; dead rows go to the slot at `rows`.
        unsafe {
            for (row, (&value, &cell)) in first.iter().zip(second).enumerate() {
                let live = u32::from(table.live(row));
                let cursor = by_first.add(value as usize + 1);
                let at = if live == 1 { *cursor } else { rows as u32 } as usize;
                *cursor += live;
                *keys.add(at) = u64::from(cell) << 32 | u64::from(ids[row]);
            }
        }
        for bucket in self.offsets.windows(2) {
            let keys = &mut self.keys[bucket[0] as usize..bucket[1] as usize];
            if let [a, b] = keys {
                (*a, *b) = (*a.min(b), *a.max(b));
            } else {
                keys.sort_unstable();
            }
        }
        let [_, high, low] = &mut self.columns;
        for ((high, low), &key) in high.iter_mut().zip(low.iter_mut()).zip(&self.keys[..rows]) {
            (*high, *low) = ((key >> 32) as u32, key as u32);
        }
        self.finish(rows);
        rows
    }
}

// The cheapest lookup found that gives `DenseTrie::ranges`'s ranges, on a
// copy of its layout: per key the offsets pair of the clamped first value,
// then for two columns one compare of the 16 cells at the range's start, with
// ranges past those cells set aside and narrowed by 64-cell masks after the
// rest, so that no branch depends on a range's length; the workload's
// buckets hold at most 64 rows. Indexing is unchecked:
// clamped values are below the offsets' length, and every block lies inside
// the padded column.
struct Bare {
    offsets: Vec<u32>,
    cells: Vec<u32>,
}

impl Bare {
    fn new(trie: &DenseTrie) -> Self {
        let (first, cells) = (trie.column(0), trie.column(1));
        let width = first.last().map_or(0, |&value| value as usize + 1);
        let mut offsets = vec![0u32; width + 2];
        for &value in first {
            offsets[value as usize + 2] += 1;
        }
        for at in 1..offsets.len() {
            offsets[at] += offsets[at - 1];
        }
        offsets.remove(0);
        offsets.push(cells.len() as u32);
        let mut cells = cells.to_vec();
        cells.resize(cells.len() + 64, 0);
        Self { offsets, cells }
    }

    fn ranges(&self, keys: &[&[u32]], out: &mut [Range<u32>], long: &mut Vec<u32>) {
        dispatch!(Level::new(), simd => self.ranges_with(simd, keys, out, long))
    }

    #[inline(always)]
    fn ranges_with<S: Simd>(
        &self,
        simd: S,
        keys: &[&[u32]],
        out: &mut [Range<u32>],
        long: &mut Vec<u32>,
    ) {
        let last = self.offsets.len() - 2;
        let (offsets, cells) = (self.offsets.as_ptr(), self.cells.as_ptr());
        // SAFETY: see `Bare`.
        unsafe {
            let first_range = |value: u32| {
                let at = offsets.add((value as usize).min(last));
                *at..*at.add(1)
            };
            let [first, second] = keys else {
                for (range, &value) in out.iter_mut().zip(keys[0]) {
                    *range = first_range(value);
                }
                return;
            };
            long.clear();
            long.reserve(out.len() + 1);
            let longs = long.as_mut_ptr();
            let mut count = 0;
            for (at, (range, (&value, &second))) in
                out.iter_mut().zip(first.iter().zip(*second)).enumerate()
            {
                let Range { start, end } = first_range(value);
                let block = std::slice::from_raw_parts(cells.add(start as usize), 16);
                let block = u32x16::from_slice(simd, block);
                let second = u32x16::splat(simd, second);
                let inside = (1u64 << (end - start).min(16)) - 1;
                let below = (block.simd_lt(second).to_bitmask() & inside).count_ones();
                let through = (block.simd_le(second).to_bitmask() & inside).count_ones();
                *range = start + below..start + through;
                *longs.add(count) = at as u32;
                count += usize::from(end - start > 16);
            }
            for &at in std::slice::from_raw_parts(longs, count) {
                let Range { start, end } = first_range(first[at as usize]);
                let second = u32x16::splat(simd, second[at as usize]);
                let (mut below, mut through) = (0u64, 0u64);
                for block in 0..4 {
                    let block_at = cells.add(start as usize + 16 * block);
                    let block_cells =
                        u32x16::from_slice(simd, std::slice::from_raw_parts(block_at, 16));
                    below |= block_cells.simd_lt(second).to_bitmask() << (16 * block);
                    through |= block_cells.simd_le(second).to_bitmask() << (16 * block);
                }
                let inside = u64::MAX >> (64 - (end - start).min(64));
                out[at as usize] =
                    start + (below & inside).count_ones()..start + (through & inside).count_ones();
            }
        }
    }
}

// An engine's many small tables over one id space: each table's first values
// span the ids of all of them, far more than its own rows.
fn thin(c: &mut Criterion, _tma: &TMAMetrics) {
    const TABLES: usize = 150;
    const ROWS: usize = 100;
    let ids = (2 * TABLES * ROWS) as u32;
    let tables: Vec<Table> = (0..TABLES)
        .map(|seed| Table::spread(ROWS, ids, seed as u64))
        .collect();
    let mut tries: Vec<DenseTrie> = tables.iter().map(Table::build).collect();
    let mut group = c.benchmark_group(format!("dense_trie/thin/{TABLES}x{ROWS}"));
    group.throughput(Throughput::Elements((TABLES * ROWS) as u64));
    group.bench_function("rebuild", |b| {
        b.iter(|| {
            for (trie, table) in tries.iter_mut().zip(&tables) {
                let columns = table.columns.each_ref().map(Vec::as_slice);
                trie.rebuild(&columns, |row| table.live(row));
            }
            black_box(&tries);
        })
    });
    let probes: Vec<[Vec<u32>; 2]> = tables
        .iter()
        .map(|table| table.probes(u64::from(ids)))
        .collect();
    let mut ranges = vec![0..0; BATCH];
    for width in [1, 2] {
        group.throughput(Throughput::Elements((TABLES * BATCH) as u64));
        group.bench_function(format!("ranges{width}"), |b| {
            b.iter(|| {
                for (trie, keys) in tries.iter().zip(&probes) {
                    let keys = [&keys[0][..BATCH], &keys[1][..BATCH]];
                    trie.ranges(black_box(&keys[..width]), &mut ranges);
                }
                black_box(&ranges);
            })
        });
    }
    group.finish();
}

criterion_group!(benches, dense_trie, thin);
criterion_main!(benches);
