use std::ops::Range;

use tir_adt::DenseTrie;
use tir_adt::simd::{hash_row, hash_rows};

use super::database::{RowIds, Rows};
use super::keymap::{RowTable, same_key};
use super::value::as_cells;
use super::{Database, IndexId, Value};

// The live rows of a table by the key columns `order`, for lookups of a
// whole key. Built on first use after the table changed.
pub(crate) struct Index {
    pub(crate) order: Vec<usize>,
    layout: Layout,
    built_at: Option<u64>,
    // The table generation and row count the layout was built from.
    built_from: (u64, usize),
}

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
    pub(crate) fn new(order: Vec<usize>) -> Self {
        let layout = Layout::Ranges {
            rows: Vec::new(),
            groups: RowTable::default(),
            bounds: vec![0],
        };
        Self {
            order,
            layout,
            built_at: None,
            built_from: (0, 0),
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
            self.layout = build_ranges(&keys, len, live);
        }
        self.built_at = Some(version);
        self.built_from = (generation, len);
    }

    // The row list the ranges from `ranges` index into.
    pub(crate) fn rows(&self) -> &[u32] {
        match &self.layout {
            Layout::DenseTrie(trie) => trie.column(self.order.len()),
            Layout::Ranges { rows, .. } => rows,
        }
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
        let ordered: Vec<&[u32]> = self.order.iter().map(|&c| &columns[c][..]).collect();
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

fn build_ranges(keys: &[&[u32]], len: usize, live: impl Fn(usize) -> bool) -> Layout {
    let key_of = |row: u32| keys.iter().map(move |column| column[row as usize]);
    let mut rows: Vec<u32> = (0..len as u32).filter(|&row| live(row as usize)).collect();
    rows.sort_unstable_by(|&a, &b| key_of(a).cmp(key_of(b)).then(a.cmp(&b)));
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
    /// The live rows whose index columns hold `key`, subsumed ones included.
    pub fn matching(&mut self, index: IndexId, key: &[Value]) -> Rows<'_> {
        let (table, at) = self.indexes[index.index()];
        let table = &mut self.tables[table.index()];
        table.refresh_index(at);
        let index = &table.indexes[at];
        assert_eq!(key.len(), index.order.len(), "a key of the index");
        let mut key = as_cells(key).to_vec();
        for (cell, column) in key.iter_mut().zip(&index.order) {
            if table.schema.ids.contains(column) {
                *cell = self.uf.find(*cell);
            }
        }
        let columns: Vec<&[u32]> = key.iter().map(std::slice::from_ref).collect();
        let mut range = 0..0;
        index.ranges(&table.columns, &columns, std::slice::from_mut(&mut range));
        let rows = &index.rows()[range.start as usize..range.end as usize];
        Rows {
            table,
            ids: RowIds::Listed(rows.iter()),
        }
    }
}
