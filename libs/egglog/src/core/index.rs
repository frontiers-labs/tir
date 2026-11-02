use std::collections::HashMap;
use std::ops::Range;

use tir_adt::simd::{hash_row, hash_rows};
use tir_adt::{DenseTrie, FxBuildHasher};

use super::database::{Row, RowIds, Rows};
use super::keymap::{RowTable, same_key};
use super::value::as_cells;
use super::{Database, IndexId, Value};

// Rows a tail takes beyond a quarter of the layout before a lookup builds the
// layout again.
const TAIL_SLACK: usize = 64;

// The live rows of a table by the columns `order`, for lookups of the first
// `key_len` of them; the rest order the rows of a key. Built on first use
// after the table changed.
#[derive(Clone)]
pub(crate) struct Index {
    pub(crate) order: Vec<usize>,
    pub(crate) key_len: usize,
    // Kept up to date by every write to the table, so a lookup needs no
    // write access.
    pub(crate) kept: bool,
    layout: Layout,
    built_at: Option<u64>,
    // The table generation and row count the layout was built from.
    built_from: (u64, usize),
    // Rows appended since the layout was built, by the hash of their key,
    // and the row count they reach: a lookup of a whole key reads these too,
    // so an edit between lookups costs its own rows.
    tail: HashMap<u32, Vec<u32>, FxBuildHasher>,
    pub(crate) tail_end: usize,
    // For a kept index by one column: the live rows of each key in the order
    // of the other columns, which writes keep sorted, so a lookup is a slice.
    groups: Option<Groups>,
}

// The live rows of each key of a kept index by one column, in order, as one
// 4-byte slot per key: a key of a dense column indexes an array of slots, any
// other key a map. A slot holds a key's only row, or the place of its rows in
// `many`, so the common key of one row costs no allocation.
#[derive(Clone, Default)]
struct Groups {
    dense: bool,
    by_slot: Vec<u32>,
    by_key: HashMap<u32, u32, FxBuildHasher>,
    many: Vec<Vec<u32>>,
}

const NO_ROWS: u32 = u32::MAX;
// Set in a slot that names a list of `many`.
const MANY: u32 = 1 << 31;

impl Groups {
    fn slot(&self, key: u32) -> u32 {
        let slot = match self.dense {
            true => self.by_slot.get(key as usize),
            false => self.by_key.get(&key),
        };
        slot.copied().unwrap_or(NO_ROWS)
    }

    fn slot_mut(&mut self, key: u32) -> &mut u32 {
        if !self.dense {
            return self.by_key.entry(key).or_insert(NO_ROWS);
        }
        if self.by_slot.len() <= key as usize {
            self.by_slot.resize(key as usize + 1, NO_ROWS);
        }
        &mut self.by_slot[key as usize]
    }

    fn rows<'a>(&'a self, slot: &'a u32) -> &'a [u32] {
        match *slot {
            NO_ROWS => &[],
            many if many & MANY != 0 => &self.many[(many & !MANY) as usize],
            _ => std::slice::from_ref(slot),
        }
    }

    fn get(&self, key: u32) -> &[u32] {
        let slot = match self.dense {
            true => self.by_slot.get(key as usize),
            false => self.by_key.get(&key),
        };
        slot.map_or(&[], |slot| self.rows(slot))
    }

    fn insert(&mut self, key: u32, at: usize, row: u32) {
        assert!(row & MANY == 0, "row ids leave the top bit free");
        let slot = *self.slot_mut(key);
        match slot {
            NO_ROWS => *self.slot_mut(key) = row,
            many if many & MANY != 0 => self.many[(many & !MANY) as usize].insert(at, row),
            one => {
                let mut rows = vec![one];
                rows.insert(at, row);
                self.many.push(rows);
                *self.slot_mut(key) = (self.many.len() - 1) as u32 | MANY;
            }
        }
    }

    fn remove(&mut self, key: u32, at: usize) {
        let slot = self.slot(key);
        match slot {
            NO_ROWS => {}
            many if many & MANY != 0 => {
                self.many[(many & !MANY) as usize].remove(at);
            }
            _ => *self.slot_mut(key) = NO_ROWS,
        }
    }

    // Every row id of every group, for renaming.
    fn rows_mut(&mut self) -> impl Iterator<Item = &mut u32> {
        let single = |slot: &&mut u32| **slot != NO_ROWS && **slot & MANY == 0;
        (self.by_slot.iter_mut().filter(single))
            .chain(self.by_key.values_mut().filter(single))
            .chain(self.many.iter_mut().flatten())
    }
}

#[derive(Clone)]
enum Layout {
    // Over the key columns and then the row ids, so the last column of the
    // trie is the row list a range indexes into.
    DenseTrie(DenseTrie),
    // Live row ids sorted by key; group `g` is `rows[bounds[g]..bounds[g + 1]]`
    // and `groups` maps a key to its group.
    Ranges {
        rows: Vec<u32>,
        groups: RowTable,
        bounds: Vec<u32>,
    },
}

impl Index {
    pub(crate) fn new(order: Vec<usize>, key_len: usize) -> Self {
        let layout = Layout::Ranges {
            rows: Vec::new(),
            groups: RowTable::default(),
            bounds: vec![0],
        };
        Self {
            order,
            key_len,
            kept: false,
            layout,
            built_at: None,
            built_from: (0, 0),
            tail: HashMap::default(),
            tail_end: 0,
            groups: None,
        }
    }

    pub(crate) fn is_current(&self, version: u64) -> bool {
        self.built_at == Some(version)
    }

    pub(crate) fn build(
        &mut self,
        columns: &[Vec<u32>],
        len: usize,
        live: impl Fn(usize) -> bool,
        dense: bool,
        (version, generation): (u64, u64),
    ) {
        let keys: Vec<&[u32]> = self.order.iter().map(|&c| &columns[c][..]).collect();
        if dense && !keys.is_empty() {
            let ids: Vec<u32> = (0..len as u32).collect();
            let mut trie_columns = keys;
            trie_columns.push(&ids);
            if let Layout::DenseTrie(trie) = &mut self.layout {
                // Rows keep their ids until the table renumbers them, so a
                // trie built in this generation only needs the rows appended
                // since.
                let (built, built_len) = self.built_from;
                if built == generation && built_len <= len {
                    trie.merge(&trie_columns, built_len, live);
                } else {
                    trie.rebuild(&trie_columns, live);
                }
            } else {
                self.layout = Layout::DenseTrie(DenseTrie::build(&trie_columns, live));
            }
        } else {
            self.layout = build_ranges(&keys, self.key_len, len, live);
        }
        self.built_at = Some(version);
        self.built_from = (generation, len);
        self.tail.clear();
        self.tail_end = len;
    }

    // Keeps the rows of each key as a sorted list from now on, starting from
    // the live rows of `columns`.
    pub(crate) fn group(
        &mut self,
        columns: &[Vec<u32>],
        len: usize,
        live: impl Fn(usize) -> bool,
        dense: bool,
    ) {
        assert_eq!(self.key_len, 1, "a grouped index has a key of one column");
        let mut groups = Groups {
            dense,
            ..Groups::default()
        };
        let rest = &self.order[1..];
        let mut rows: Vec<u32> = (0..len as u32).filter(|&row| live(row as usize)).collect();
        rows.sort_unstable_by_key(|&row| {
            (
                columns[self.order[0]][row as usize],
                order_of(columns, rest, row),
            )
        });
        for row in rows {
            let key = columns[self.order[0]][row as usize];
            let at = groups.get(key).len();
            groups.insert(key, at, row);
        }
        self.groups = Some(groups);
    }

    pub(crate) fn is_dense_group(&self) -> bool {
        self.groups.as_ref().is_some_and(|groups| groups.dense)
    }

    pub(crate) fn is_grouped(&self) -> bool {
        self.groups.is_some()
    }

    // Puts a new live `row` into its group.
    pub(crate) fn group_insert(&mut self, columns: &[Vec<u32>], row: u32) {
        let (key, rest) = (columns[self.order[0]][row as usize], &self.order[1..]);
        let groups = self.groups.as_mut().expect("a grouped index");
        let order = order_of(columns, rest, row);
        let rows = groups.get(key);
        // A new row mostly sorts last: an appended op, a use by a new op.
        let at = match rows.last() {
            Some(&last) if order_of(columns, rest, last) > order => {
                rows.partition_point(|&held| order_of(columns, rest, held) < order)
            }
            _ => rows.len(),
        };
        groups.insert(key, at, row);
    }

    // Takes a `row` that stops being live out of its group.
    pub(crate) fn group_remove(&mut self, columns: &[Vec<u32>], row: u32) {
        let (key, rest) = (columns[self.order[0]][row as usize], &self.order[1..]);
        let groups = self.groups.as_mut().expect("a grouped index");
        let order = order_of(columns, rest, row);
        let rows = groups.get(key);
        let at = rows.partition_point(|&held| order_of(columns, rest, held) < order);
        if rows.get(at) == Some(&row) {
            groups.remove(key, at);
        }
    }

    // Renames the rows of every group after a compaction: `renamed[old]` is
    // the new id of a live row. Renaming keeps the order of ids.
    pub(crate) fn group_rename(&mut self, renamed: &[u32]) {
        let groups = self.groups.as_mut().expect("a grouped index");
        for row in groups.rows_mut() {
            *row = renamed[*row as usize];
        }
    }

    // Whether the rows of the table up to `len` can be reached by appending
    // to the tail, in table generation `generation`.
    pub(crate) fn can_catch_up(&self, generation: u64, len: usize) -> bool {
        let (built, built_len) = self.built_from;
        self.built_at.is_some()
            && built == generation
            && len - built_len <= built_len / 4 + TAIL_SLACK
    }

    // Puts the rows from the end of the tail up to `len` into the tail.
    pub(crate) fn catch_up(&mut self, columns: &[Vec<u32>], len: usize) {
        let key_columns: Vec<&[u32]> = (self.order[..self.key_len].iter())
            .map(|&c| &columns[c][..])
            .collect();
        let rest = &self.order[self.key_len..];
        let mut key = Vec::with_capacity(self.key_len);
        for row in self.tail_end..len {
            key.clear();
            key.extend(key_columns.iter().map(|column| column[row]));
            let rows = self.tail.entry(hash_row(&key)).or_default();
            // Each bucket stays in the order of the other columns, so a
            // lookup merges it with the layout instead of sorting.
            let at = rows.partition_point(|&held| {
                order_of(columns, rest, held) <= order_of(columns, rest, row as u32)
            });
            rows.insert(at, row as u32);
        }
        self.tail_end = len;
    }

    // The row list the ranges from `ranges` index into.
    pub(crate) fn rows(&self) -> &[u32] {
        match &self.layout {
            Layout::DenseTrie(trie) => trie.column(self.order.len()),
            Layout::Ranges { rows, .. } => rows,
        }
    }

    // The rows of the layout whose key is `key`, some of which may have died
    // since it was built; `tail_rows` has the rest.
    pub(crate) fn laid_rows(&self, columns: &[Vec<u32>], key: &[u32]) -> &[u32] {
        let cells: Vec<&[u32]> = key.iter().map(std::slice::from_ref).collect();
        let mut range = 0..0;
        self.ranges(columns, &cells, std::slice::from_mut(&mut range));
        &self.rows()[range.start as usize..range.end as usize]
    }

    // The rows of the tail whose key is `key`.
    pub(crate) fn tail_rows<'a>(
        &'a self,
        columns: &'a [Vec<u32>],
        key: &'a [u32],
    ) -> impl Iterator<Item = u32> + 'a {
        let rows = self.tail.get(&hash_row(key)).map_or(&[][..], Vec::as_slice);
        let key_columns = &self.order[..self.key_len];
        rows.iter().copied().filter(move |&row| {
            (key_columns.iter().zip(key)).all(|(&c, &cell)| columns[c][row as usize] == cell)
        })
    }

    // `keys` holds one column per key column; key `i` gets `out[i]`.
    pub(crate) fn ranges(&self, columns: &[Vec<u32>], keys: &[&[u32]], out: &mut [Range<u32>]) {
        let (rows, groups, bounds) = match &self.layout {
            Layout::DenseTrie(trie) => return trie.ranges(keys, out),
            Layout::Ranges {
                rows,
                groups,
                bounds,
            } => (rows, groups, bounds),
        };
        let mut hashes = vec![0; out.len()];
        hash_rows(keys, &mut hashes);
        let ordered: Vec<&[u32]> = (self.order[..self.key_len].iter())
            .map(|&c| &columns[c][..])
            .collect();
        let mut key = vec![0; keys.len()];
        for (i, range) in out.iter_mut().enumerate() {
            for (cell, column) in key.iter_mut().zip(keys) {
                *cell = column[i];
            }
            let first_row = |g: u32| rows[bounds[g as usize] as usize];
            *range = match groups.get(hashes[i], |g| same_key(&ordered, first_row(g), &key)) {
                Some(g) => bounds[g as usize]..bounds[g as usize + 1],
                None => 0..0,
            };
        }
    }
}

// Rows sorted by all of `columns`; a group shares the first `key_len`.
fn build_ranges(
    columns: &[&[u32]],
    key_len: usize,
    len: usize,
    live: impl Fn(usize) -> bool,
) -> Layout {
    let cells_of = |row: u32| columns.iter().map(move |column| column[row as usize]);
    let key_of = |row: u32| cells_of(row).take(key_len);
    let mut rows: Vec<u32> = (0..len as u32).filter(|&row| live(row as usize)).collect();
    if columns.len() <= 3 {
        // The cells of up to three columns and the row id fit one u128 whose
        // order is the order of the rows, so the sort compares integers.
        let packed = |row: u32| {
            let cells = cells_of(row).fold(0u128, |key, cell| key << 32 | u128::from(cell));
            cells << 32 | u128::from(row)
        };
        let mut keys: Vec<u128> = rows.iter().map(|&row| packed(row)).collect();
        keys.sort_unstable();
        rows = keys.into_iter().map(|key| key as u32).collect();
    } else {
        rows.sort_unstable_by(|&a, &b| cells_of(a).cmp(cells_of(b)).then(a.cmp(&b)));
    }
    let starts: Vec<usize> = (0..rows.len())
        .filter(|&at| at == 0 || !key_of(rows[at]).eq(key_of(rows[at - 1])))
        .collect();
    let mut groups = RowTable::default();
    let mut key = Vec::new();
    for (group, &start) in starts.iter().enumerate() {
        key.clear();
        key.extend(key_of(rows[start]));
        groups.insert(hash_row(&key), group as u32, |_| false);
    }
    let mut bounds: Vec<u32> = starts.iter().map(|&start| start as u32).collect();
    bounds.push(rows.len() as u32);
    Layout::Ranges {
        rows,
        groups,
        bounds,
    }
}

impl Database {
    /// The live rows whose key columns hold `key`, subsumed ones included,
    /// in the order of the index's other columns.
    pub fn matching(&mut self, index: IndexId, key: &[Value]) -> Rows<'_> {
        let (table, at) = self.indexes[index.index()];
        self.tables[table.index()].catch_up_index(at);
        self.listed(index, key)
    }

    // The table of a kept index by one column, and the live rows it lists
    // for `key` in order, if it groups them.
    fn group(&self, index: IndexId, key: &[Value]) -> (&super::table::Table, Option<&[u32]>) {
        let (table, at) = self.indexes[index.index()];
        let table = &self.tables[table.index()];
        let index = &table.indexes[at];
        let Some(groups) = &index.groups else {
            return (table, None);
        };
        let mut cell = as_cells(key)[0];
        if table.schema.ids.contains(&index.order[0]) {
            cell = self.uf.find(cell);
        }
        (table, Some(groups.get(cell)))
    }

    /// How many rows [`Database::listed`] gives for `key`, through a kept
    /// index by one column.
    pub fn listed_len(&self, index: IndexId, key: &[Value]) -> usize {
        self.group(index, key)
            .1
            .expect("a kept index by one column")
            .len()
    }

    /// The `n`th row [`Database::listed`] gives for `key`, through a kept
    /// index by one column.
    pub fn listed_nth(&self, index: IndexId, key: &[Value], n: usize) -> Option<Row<'_>> {
        let (table, rows) = self.group(index, key);
        let row = *rows.expect("a kept index by one column").get(n)?;
        Some(Row::new(table, row, 0))
    }

    /// [`Database::matching`] through an index from
    /// [`Database::add_kept_index`], which needs no write access.
    pub fn listed(&self, index: IndexId, key: &[Value]) -> Rows<'_> {
        if let (table, Some(rows)) = self.group(index, key) {
            return Rows {
                table,
                ids: RowIds::Listed(rows.iter()),
            };
        }
        let (table, at) = self.indexes[index.index()];
        let table = &self.tables[table.index()];
        let index = &table.indexes[at];
        assert!(
            index.kept || index.tail_end == table.len(),
            "a lookup without write access goes through a kept index"
        );
        assert_eq!(key.len(), index.key_len, "a key of the index");
        let mut key = as_cells(key).to_vec();
        for (cell, column) in key.iter_mut().zip(&index.order) {
            if table.schema.ids.contains(column) {
                *cell = self.uf.find(*cell);
            }
        }
        let rows = index.laid_rows(&table.columns, &key);
        let mut tail = index.tail_rows(&table.columns, &key).peekable();
        if tail.peek().is_none() {
            return Rows {
                table,
                ids: RowIds::Listed(rows.iter()),
            };
        }
        let rest = &index.order[index.key_len..];
        let rows = merge(rows.iter().copied(), tail, |row| {
            order_of(&table.columns, rest, row)
        });
        Rows {
            table,
            ids: RowIds::Owned(rows.into_iter()),
        }
    }
}

// Where `row` sorts among the rows of its key: the cells of the columns
// `rest`, then the row id, packed as `build_ranges` packs them.
fn order_of(columns: &[Vec<u32>], rest: &[usize], row: u32) -> u128 {
    assert!(
        rest.len() <= 3,
        "an index orders a key's rows by three columns at most"
    );
    let cells = (rest.iter()).fold(0u128, |key, &c| {
        key << 32 | u128::from(columns[c][row as usize])
    });
    cells << 32 | u128::from(row)
}

// The rows of two runs that are each in `order`, in that order.
fn merge<K: Ord>(
    laid: impl Iterator<Item = u32>,
    tail: impl Iterator<Item = u32>,
    order: impl Fn(u32) -> K,
) -> Vec<u32> {
    let (mut laid, mut tail) = (laid.peekable(), tail.peekable());
    let mut rows = Vec::new();
    loop {
        let next = match (laid.peek(), tail.peek()) {
            (Some(&a), Some(&b)) if order(a) <= order(b) => laid.next(),
            (Some(_), Some(_)) => tail.next(),
            (Some(_), None) => laid.next(),
            (None, Some(_)) => tail.next(),
            (None, None) => return rows,
        };
        rows.extend(next);
    }
}
