use std::ops::Range;

use tir_adt::simd::{hash_row, hash_rows};

use super::index::Index;
use super::keymap::KeyMap;
use super::schema::{Shape, TableSchema};
use super::uf::UnionFind;
use super::value::{Value, as_cells};
use crate::TableId;

// A columnar row log. Rows only append: a row whose output changes is killed
// and its new version appended with the current timestamp, so the rows
// written since a timestamp are a suffix of the log.
pub(crate) struct Table {
    pub(crate) schema: TableSchema,
    // Key columns, then the output column.
    pub(crate) columns: Vec<Vec<u32>>,
    // Ascending.
    ts: Vec<u32>,
    live: Vec<u64>,
    subsumed: Vec<u64>,
    keys: KeyMap,
    pub(crate) indexes: Vec<Index>,
    dead: usize,
    // Bumped by every change to the set of live rows; an index built at an
    // older version is stale.
    version: u64,
    // Bumped whenever row ids change meaning.
    renumbered: u32,
    // Whether an index is kept, so a write brings it up to date.
    keeps_indexes: bool,
    // The column whose cell a killed row leaves for the table logging it,
    // and the cells left since the log last took them.
    pub(crate) retire: Option<(usize, TableId)>,
    pub(crate) retired: Vec<u32>,
}

// A clone rebuilds its key map, copies its kept indexes, and builds the
// others on first use. Rows keep their ids, so the copies stay valid.
impl Clone for Table {
    fn clone(&self) -> Self {
        let mut table = Self {
            schema: self.schema.clone(),
            columns: self.columns.clone(),
            ts: self.ts.clone(),
            live: self.live.clone(),
            subsumed: self.subsumed.clone(),
            keys: KeyMap::new(self.schema.keys, 0),
            indexes: (self.indexes.iter())
                .map(|index| match index.kept {
                    true => index.clone(),
                    false => Index::new(index.order.clone(), index.key_len),
                })
                .collect(),
            dead: self.dead,
            version: self.version,
            renumbered: 0,
            keeps_indexes: self.keeps_indexes,
            retire: self.retire,
            retired: self.retired.clone(),
        };
        table.rebuild_keys();
        table.renumbered = self.renumbered;
        table
    }
}

impl Table {
    pub(crate) fn new(schema: TableSchema) -> Self {
        Self {
            columns: vec![Vec::new(); schema.columns.len()],
            ts: Vec::new(),
            live: Vec::new(),
            subsumed: Vec::new(),
            keys: KeyMap::new(schema.keys, 0),
            indexes: Vec::new(),
            dead: 0,
            version: 0,
            renumbered: 0,
            keeps_indexes: false,
            retire: None,
            retired: Vec::new(),
            schema,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.ts.len()
    }

    pub(crate) fn is_live(&self, row: u32) -> bool {
        bit(&self.live, row)
    }

    // One bit per row.
    pub(crate) fn live(&self) -> &[u64] {
        &self.live
    }

    pub(crate) fn is_subsumed(&self, row: u32) -> bool {
        bit(&self.subsumed, row)
    }

    pub(crate) fn timestamp(&self, row: u32) -> u32 {
        self.ts[row as usize]
    }

    pub(crate) fn cell(&self, row: u32, column: usize) -> u32 {
        self.columns[column][row as usize]
    }

    // Whether no live row holds an id from `mark` on.
    pub(crate) fn holds_none_from(&self, mark: u32) -> bool {
        (0..self.len() as u32)
            .filter(|&row| self.is_live(row))
            .all(|row| (self.schema.ids.iter()).all(|&column| self.cell(row, column) < mark))
    }

    // The rows written at `ts` or later, live or not.
    pub(crate) fn since(&self, ts: u32) -> Range<u32> {
        self.ts.partition_point(|&t| t < ts) as u32..self.len() as u32
    }

    // `key` must be canonical and `hash` its `hash_row`.
    pub(crate) fn find(&self, key: &[u32], hash: u32) -> Option<u32> {
        self.keys.get(key, hash, &self.columns)
    }

    // `find` for a batch of keys by column, into `out`, EMPTY where missing.
    pub(crate) fn find_many(&self, keys: &[&[u32]], hashes: &[u32], out: &mut [u32]) {
        self.keys.get_many(keys, hashes, &self.columns, out);
    }

    // The cells of `values` with every id canonical.
    pub(crate) fn canonical(&self, values: &[Value], uf: &UnionFind) -> Vec<u32> {
        let mut cells = as_cells(values).to_vec();
        for &column in &self.schema.ids {
            if let Some(cell) = cells.get_mut(column) {
                *cell = uf.find(*cell);
            }
        }
        cells
    }

    pub(crate) fn canonical_key(&self, key: &[Value], uf: &UnionFind) -> Vec<u32> {
        assert_eq!(
            key.len(),
            self.schema.keys,
            "a key of `{}`",
            self.schema.name
        );
        self.canonical(key, uf)
    }

    pub(crate) fn lookup(&self, key: &[Value], uf: &UnionFind) -> Option<Value> {
        let key = self.canonical_key(key, uf);
        let row = self.find(&key, hash_row(&key))?;
        let output = self.columns.get(self.schema.keys);
        Some(Value::new(output.map_or(0, |column| column[row as usize])))
    }

    pub(crate) fn push(&mut self, row: &[u32], hash: u32, ts: u32, subsumed: bool) -> u32 {
        let id = u32::try_from(self.len()).expect("rows fit in u32");
        for (column, &cell) in self.columns.iter_mut().zip(row) {
            column.push(cell);
        }
        self.ts.push(ts);
        if id % 64 == 0 {
            self.live.push(0);
            self.subsumed.push(0);
        }
        set_bit(&mut self.live, id, true);
        set_bit(&mut self.subsumed, id, subsumed);
        self.keys
            .insert(&row[..self.schema.keys], hash, id, &self.columns);
        self.version += 1;
        self.group_new_row(id);
        self.keep_indexes();
        id
    }

    // Kills the live row and drops its key, whose hash is `hash`, from the
    // key map.
    pub(crate) fn remove(&mut self, row: u32, hash: u32) {
        self.keys.remove(row, hash);
        self.kill(row);
    }

    // Kills a row whose key the key map has moved to a newer row.
    pub(crate) fn kill(&mut self, row: u32) {
        if self.keeps_indexes {
            for index in &mut self.indexes {
                if index.is_grouped() {
                    index.group_remove(&self.columns, row);
                }
            }
        }
        if let Some((column, _)) = self.retire {
            self.retired.push(self.columns[column][row as usize]);
        }
        set_bit(&mut self.live, row, false);
        self.dead += 1;
        self.version += 1;
    }

    pub(crate) fn subsume(&mut self, row: u32) {
        if !self.is_subsumed(row) {
            set_bit(&mut self.subsumed, row, true);
            self.version += 1;
        }
    }

    // Changes whenever the rows change, subsumption included.
    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    // `cells`, a row or a key, then its other orders under the symmetries.
    pub(crate) fn orders(&self, cells: &[u32]) -> Vec<Vec<u32>> {
        let permute = |p: &Vec<usize>| p[..cells.len()].iter().map(|&c| cells[c]).collect();
        let mut orders = vec![cells.to_vec()];
        orders.extend(self.schema.symmetries.iter().map(permute));
        orders
    }

    pub(crate) fn clear(&mut self) {
        self.columns.iter_mut().for_each(Vec::clear);
        self.ts.clear();
        self.live.clear();
        self.subsumed.clear();
        self.dead = 0;
        self.version += 1;
        self.rebuild_keys();
        for index in self.indexes.iter_mut().filter(|index| index.is_grouped()) {
            let dense = index.is_dense_group();
            index.group(&[], 0, |_| false, dense);
        }
        self.keep_indexes();
    }

    // Indexes pick their layout from the shapes when built.
    pub(crate) fn set_shapes(&mut self, shapes: &[Shape]) {
        self.schema.shapes = shapes.to_vec();
        self.version += 1;
    }

    // Drops the dead rows once they are most of the log.
    pub(crate) fn maybe_compact(&mut self) {
        if self.dead * 2 <= self.len() {
            return;
        }
        let rows: Vec<usize> = (0..self.len())
            .filter(|&row| self.is_live(row as u32))
            .collect();
        if self.keeps_indexes {
            let mut renamed = vec![u32::MAX; self.len()];
            for (new, &old) in rows.iter().enumerate() {
                renamed[old] = new as u32;
            }
            for index in self.indexes.iter_mut().filter(|index| index.is_grouped()) {
                index.group_rename(&renamed);
            }
        }
        for column in &mut self.columns {
            *column = rows.iter().map(|&row| column[row]).collect();
        }
        self.ts = rows.iter().map(|&row| self.ts[row]).collect();
        let subsumed: Vec<bool> = rows
            .iter()
            .map(|&row| self.is_subsumed(row as u32))
            .collect();
        self.live = vec![0; rows.len().div_ceil(64)];
        self.subsumed = vec![0; rows.len().div_ceil(64)];
        for (row, subsumed) in subsumed.into_iter().enumerate() {
            set_bit(&mut self.live, row as u32, true);
            set_bit(&mut self.subsumed, row as u32, subsumed);
        }
        self.dead = 0;
        self.rebuild_keys();
        self.keep_indexes();
    }

    fn rebuild_keys(&mut self) {
        self.keys = KeyMap::new(self.schema.keys, self.len() - self.dead);
        let keys: Vec<&[u32]> = self.columns[..self.schema.keys]
            .iter()
            .map(Vec::as_slice)
            .collect();
        let mut hashes = vec![0; self.len()];
        hash_rows(&keys, &mut hashes);
        let mut key = Vec::with_capacity(keys.len());
        for (row, &hash) in hashes.iter().enumerate() {
            if self.is_live(row as u32) {
                key.clear();
                key.extend(keys.iter().map(|column| column[row]));
                self.keys.insert(&key, hash, row as u32, &self.columns);
            }
        }
        self.version += 1;
        self.renumbered += 1;
    }

    pub(crate) fn renumbered(&self) -> u32 {
        self.renumbered
    }

    pub(crate) fn add_index(&mut self, order: &[usize]) -> usize {
        self.add_ordered_index(order, order.len())
    }

    // An index by the first `key_len` columns of `order`, whose rows of a
    // key are in the order of the rest.
    pub(crate) fn add_ordered_index(&mut self, order: &[usize], key_len: usize) -> usize {
        let same = |index: &Index| index.order == order && index.key_len == key_len;
        if let Some(at) = self.indexes.iter().position(same) {
            return at;
        }
        // A kept index by the same key answers a lookup of that key, and
        // every write keeps it current.
        let kept = |index: &Index| {
            index.is_grouped()
                && index.key_len == key_len
                && index.order[..key_len] == order[..key_len]
        };
        if order.len() == key_len
            && let Some(at) = self.indexes.iter().position(kept)
        {
            return at;
        }
        self.indexes.push(Index::new(order.to_vec(), key_len));
        self.indexes.len() - 1
    }

    // A kept index by the first `key_len` columns of `order`, built now and
    // brought up to date by every write.
    pub(crate) fn add_kept_index(&mut self, order: &[usize], key_len: usize) -> usize {
        let at = self.add_ordered_index(order, key_len);
        self.indexes[at].kept = true;
        self.keeps_indexes = true;
        if key_len == 1 && !self.indexes[at].is_grouped() {
            let (len, live) = (self.len(), self.live.clone());
            let dense = self.schema.shapes[order[0]] == Shape::Dense;
            self.indexes[at].group(&self.columns, len, |row| bit(&live, row as u32), dense);
        } else {
            self.catch_up_index(at);
        }
        at
    }

    fn keep_indexes(&mut self) {
        if !self.keeps_indexes {
            return;
        }
        for at in 0..self.indexes.len() {
            if self.indexes[at].kept && !self.indexes[at].is_grouped() {
                self.catch_up_index(at);
            }
        }
    }

    // Puts the row `push` just wrote into every grouped index.
    fn group_new_row(&mut self, row: u32) {
        if !self.keeps_indexes {
            return;
        }
        for index in self.indexes.iter_mut().filter(|index| index.is_grouped()) {
            index.group_insert(&self.columns, row);
        }
    }

    // A declared index whose key is `column` alone.
    pub(crate) fn id_index(&self, column: usize) -> Option<usize> {
        (self.indexes.iter()).position(|index| index.key_len == 1 && index.order[0] == column)
    }

    // Brings index `at` up to date for a lookup of a whole key: rows
    // appended since its layout go to its tail, and the layout is built again
    // when they outgrow it or when row ids changed meaning. A killed row
    // stays in the layout, so the reader skips rows that are not live.
    pub(crate) fn catch_up_index(&mut self, at: usize) {
        let len = self.len();
        let index = &mut self.indexes[at];
        if index.is_current(self.version) {
            return;
        }
        if index.can_catch_up(u64::from(self.renumbered), len) {
            index.catch_up(&self.columns, len);
        } else {
            self.refresh_index(at);
        }
    }

    // Brings index `at` up to date for a search outside colors, which reads
    // a grouped index's groups and needs no layout of it.
    pub(crate) fn refresh_for_search(&mut self, at: usize) {
        if !self.indexes[at].is_grouped() {
            self.refresh_index(at);
        }
    }

    pub(crate) fn refresh_index(&mut self, at: usize) {
        let index = &self.indexes[at];
        if index.is_current(self.version) {
            return;
        }
        let dense = index
            .order
            .first()
            .is_some_and(|&c| self.schema.shapes[c] == Shape::Dense);
        let mut index = std::mem::replace(&mut self.indexes[at], Index::new(Vec::new(), 0));
        index.build(
            &self.columns,
            self.len(),
            |row| self.is_live(row as u32),
            dense,
            (self.version, u64::from(self.renumbered)),
        );
        self.indexes[at] = index;
    }
}

fn bit(words: &[u64], at: u32) -> bool {
    words[at as usize / 64] >> (at % 64) & 1 != 0
}

fn set_bit(words: &mut [u64], at: u32, value: bool) {
    let word = &mut words[at as usize / 64];
    *word = *word & !(1 << (at % 64)) | u64::from(value) << (at % 64);
}
