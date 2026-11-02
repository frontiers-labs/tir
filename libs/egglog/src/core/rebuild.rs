use tir_adt::simd::{and_words, hash_row, hash_rows, select_moved};

use super::database::{PrimCtx, Write};
use super::keymap::{EMPTY, probe_batch};
use super::schema::close_group;
use super::table::Table;
use super::{Database, Error, Merge, TableId, TableKind, Value};

// Rows to write into one table, by column.
pub(crate) struct Delta {
    pub(crate) columns: Vec<Vec<u32>>,
    pub(crate) subsumed: Vec<bool>,
}

impl Database {
    /// Bring every table back to canonical form after unions (thesis §3.5.7):
    /// rows that hold a non-canonical id are written again with canonical
    /// ids, which merges rows whose keys become equal, which may union
    /// more ids. Containers are canonicalized alongside. Ends when no union
    /// is left to apply, then starts a new timestamp.
    pub fn rebuild(&mut self) -> Result<(), Error> {
        let mut moved_any = false;
        loop {
            self.flush()?;
            self.unite_written();
            if self.uf.unions() == 0 {
                break;
            }
            moved_any = true;
            let mut moved = self.uf.take_moved();
            let dirty = self.containers.rebuild(&mut self.uf);
            if self.local_rebuild_pays(moved.len() + dirty.len()) {
                moved.extend_from_slice(&dirty);
                let deltas: Vec<Delta> = (self.tables.iter_mut())
                    .map(|table| {
                        let rows = rows_holding(table, &moved);
                        take_rows(table, &rows)
                    })
                    .collect();
                self.apply_moved(deltas)?;
                continue;
            }
            let mut marked = Vec::new();
            let map = if dirty.is_empty() {
                self.uf.parents()
            } else {
                // A dirty container keeps its id, so its rows are written
                // again as if it had moved, for a new timestamp.
                marked.extend_from_slice(self.uf.parents());
                for id in dirty {
                    marked[id as usize] = u32::MAX;
                }
                &marked
            };
            let mut hits = Vec::new();
            let deltas: Vec<Delta> = (self.tables.iter_mut())
                .map(|table| take_moved(table, map, &mut hits))
                .collect();
            self.apply_moved(deltas)?;
        }
        self.now += 1;
        if !self.colors.is_empty() {
            self.rebase_colors(moved_any)?;
        }
        Ok(())
    }

    // Writes again the rows that held moved ids, one delta per table, then
    // unites the keyed groups those rows made equal.
    fn apply_moved(&mut self, deltas: Vec<Delta>) -> Result<(), Error> {
        let mut touched = Vec::new();
        for (table, mut delta) in deltas.into_iter().enumerate() {
            let table = TableId::new(table);
            self.apply(table, &mut delta)?;
            self.touched_groups(table, &delta, &mut touched);
        }
        self.unite_congruent(super::Color::BASE, touched);
        Ok(())
    }

    // Whether finding the rows of `moved` ids through the indexes of the id
    // columns costs less than scanning every id cell. A table that has rows
    // needs a declared index led by each of its id columns: a client that
    // edits rows declares them, and building them for a database that only
    // runs rules would cost more than the scans they save.
    fn local_rebuild_pays(&self, moved: usize) -> bool {
        let mut lookups = 0;
        let mut cells = 0;
        for table in self.tables.iter().filter(|table| table.len() > 0) {
            for &column in &table.schema.ids {
                if table.id_index(column).is_none() {
                    return false;
                }
            }
            lookups += table.schema.ids.len() * moved;
            cells += table.schema.ids.len() * table.len();
        }
        lookups * LOOKUP_CELLS < cells
    }

    fn flush(&mut self) -> Result<(), Error> {
        while !self.pending.is_empty() {
            for write in std::mem::take(&mut self.pending) {
                match write {
                    Write::Insert(table, row) => self.insert(table, &row)?,
                    Write::Remove(table, key) => self.remove(table, &key),
                    Write::Subsume(table, key) => self.subsume(table, &key),
                }
            }
        }
        self.predicted.clear();
        Ok(())
    }

    pub(crate) fn apply(&mut self, table: TableId, delta: &mut Delta) -> Result<(), Error> {
        let schema = &self.tables[table.index()].schema;
        for &column in &schema.ids {
            for cell in &mut delta.columns[column] {
                *cell = self.uf.find(*cell);
            }
        }
        let mut hashes = vec![0; delta.subsumed.len()];
        let keys: Vec<&[u32]> = (delta.columns[..schema.keys].iter())
            .map(Vec::as_slice)
            .collect();
        hash_rows(&keys, &mut hashes);
        // The rows of a chunk of keys are found together, so the cache misses
        // overlap. A found row holds while it is live and rows keep their
        // ids; a key found missing, while no earlier row of the chunk may have
        // written it, which a filter of four bits per row tells. Other orders
        // of a row write other keys, so a table with symmetries looks each key
        // up again.
        let symmetric = !schema.symmetries.is_empty();
        let size = probe_batch(schema.keys);
        let mut found = vec![EMPTY; size];
        let mut written = vec![0u64; size.div_ceil(16)];
        let bits = written.len() as u64 * 64;
        let mut row = Vec::with_capacity(delta.columns.len());
        for start in (0..hashes.len()).step_by(size) {
            let chunk = start..(start + size).min(hashes.len());
            let t = &self.tables[table.index()];
            let keys: Vec<&[u32]> = (delta.columns[..t.schema.keys].iter())
                .map(|column| &column[chunk.clone()])
                .collect();
            t.find_many(&keys, &hashes[chunk.clone()], &mut found[..chunk.len()]);
            let renumbered = t.renumbered();
            written.fill(0);
            for (i, &at) in chunk.zip(&found) {
                let t = &self.tables[table.index()];
                let bit = ((u64::from(hashes[i]) * bits) >> 32) as usize;
                let (word, bit) = (bit / 64, bit % 64);
                let known = !symmetric
                    && t.renumbered() == renumbered
                    && match at {
                        EMPTY => written[word] >> bit & 1 == 0,
                        at => t.is_live(at),
                    };
                written[word] |= 1 << bit;
                row.clear();
                row.extend(delta.columns.iter().map(|column| column[i]));
                self.apply_row(
                    table,
                    &row,
                    hashes[i],
                    delta.subsumed[i],
                    known.then_some(at),
                )?;
            }
        }
        self.tables[table.index()].maybe_compact();
        Ok(())
    }

    // `row` must be canonical and `hash` the hash of its key. `at` is the
    // row of the key or EMPTY, if the caller knows. Writes the other orders
    // of the row too.
    pub(crate) fn apply_row(
        &mut self,
        t: TableId,
        row: &[u32],
        hash: u32,
        subsumed: bool,
        at: Option<u32>,
    ) -> Result<(), Error> {
        self.apply_one(t, row, hash, subsumed, at)?;
        let table = &self.tables[t.index()];
        if table.schema.symmetries.is_empty() {
            return Ok(());
        }
        let keys = table.schema.keys;
        for order in table.orders(row).into_iter().skip(1) {
            self.apply_one(t, &order, hash_row(&order[..keys]), subsumed, None)?;
        }
        Ok(())
    }

    fn apply_one(
        &mut self,
        t: TableId,
        row: &[u32],
        hash: u32,
        subsumed: bool,
        at: Option<u32>,
    ) -> Result<(), Error> {
        if let Some(Some(group)) = self.group_of_table.get(t.index()) {
            self.written_groups.push((*group, row[0]));
        }
        let now = self.now;
        let table = &mut self.tables[t.index()];
        let keys = table.schema.keys;
        let at = at.unwrap_or_else(|| table.find(&row[..keys], hash).unwrap_or(EMPTY));
        if at == EMPTY {
            table.push(row, hash, now, subsumed);
            return Ok(());
        }
        let subsumed = subsumed || table.is_subsumed(at);
        let merged = if keys == row.len() {
            None
        } else {
            let mut old = table.cell(at, keys);
            if table.schema.ids.contains(&keys) {
                old = self.uf.find(old);
            }
            let (old, new) = (Value::new(old), Value::new(row[keys]));
            // A merge by a primitive or to a constant may change equal
            // outputs, as `(+ old new)` does; the others keep them.
            let eager = matches!(table.schema.merge, Some(Merge::Prim(..) | Merge::Const(_)));
            Some(if old == new && !eager {
                old
            } else {
                self.merge(t, old, new)?
            })
        };
        let table = &mut self.tables[t.index()];
        match merged {
            Some(merged) if merged.raw() != table.cell(at, keys) => {
                table.kill(at);
                let mut row = row.to_vec();
                row[keys] = merged.raw();
                table.push(&row, hash, now, subsumed);
                table.maybe_compact();
            }
            _ if subsumed => table.subsume(at),
            _ => {}
        }
        Ok(())
    }

    /// The output for `key`. A constructor gives a missing key a fresh id and
    /// a relation adds the missing row; a function has nothing to make.
    pub fn entry(&mut self, table: TableId, key: &[Value]) -> Result<Value, Error> {
        let schema = &self.tables[table.index()].schema;
        let (name, kind, keys) = (schema.name.clone(), schema.kind, schema.keys);
        if key.len() != keys {
            let found = key.len();
            return Err(Error::Arity {
                table: name,
                expected: keys,
                found,
            });
        }
        if let Some(value) = self.lookup(table, key) {
            return Ok(value);
        }
        match kind {
            TableKind::Function => {
                let default = self.tables[table.index()].schema.default;
                let default = default.ok_or(Error::Missing { table: name })?;
                self.insert(table, &[key, &[default]].concat())?;
                Ok(default)
            }
            TableKind::Relation => self.insert(table, key).map(|()| Value::new(0)),
            TableKind::Constructor => {
                let id = Value::new(self.uf.fresh());
                self.insert(table, &[key, &[id]].concat())?;
                Ok(id)
            }
        }
    }

    // Adds the symmetry `order` with what it generates together with the
    // symmetries the table has, and writes the new orders of every row.
    pub(crate) fn add_symmetry(&mut self, table: TableId, order: &[usize]) -> Result<(), Error> {
        if !close_group(&mut self.tables[table.index()].schema.symmetries, order) {
            return Ok(());
        }
        let t = &self.tables[table.index()];
        let rows: Vec<(Vec<u32>, bool)> = (0..t.len() as u32)
            .filter(|&row| t.is_live(row))
            .map(|row| {
                let cells: Vec<Value> = (0..t.columns.len())
                    .map(|c| Value::new(t.cell(row, c)))
                    .collect();
                (t.canonical(&cells, &self.uf), t.is_subsumed(row))
            })
            .collect();
        let keys = t.schema.keys;
        for (row, subsumed) in rows {
            self.apply_row(table, &row, hash_row(&row[..keys]), subsumed, None)?;
        }
        Ok(())
    }

    // Grows with every change to the rows of any table.
    pub(crate) fn changes(&self) -> u64 {
        self.tables.iter().map(Table::version).sum()
    }

    fn merge(&mut self, table: TableId, old: Value, new: Value) -> Result<Value, Error> {
        let mut ctx = self.ctx();
        let tables = ctx.tables;
        let schema = &tables[table.index()].schema;
        let merge = schema.merge.as_ref().expect("a function has a merge");
        eval(merge, &mut ctx, old, new).ok_or_else(|| Error::Conflict {
            table: schema.name.clone(),
            old,
            new,
        })
    }
}

fn eval(merge: &Merge, ctx: &mut PrimCtx<'_>, old: Value, new: Value) -> Option<Value> {
    let eval_all = |args: &[Merge], ctx: &mut PrimCtx<'_>| -> Option<Vec<Value>> {
        args.iter().map(|arg| eval(arg, ctx, old, new)).collect()
    };
    match merge {
        Merge::AssertEq => (old == new).then_some(old),
        Merge::Union => Some(Value::new(ctx.uf.union(old.raw(), new.raw()))),
        Merge::Old => Some(old),
        Merge::New => Some(new),
        Merge::Const(value) => Some(*value),
        Merge::Prim(prim, args) => {
            let args = eval_all(args, ctx)?;
            ctx.call(*prim, &args)
        }
        Merge::Function(table, args) => {
            let key = eval_all(args, ctx)?;
            ctx.entry(*table, &key)
        }
    }
}

// Cells a scan reads in the time of one index lookup, about.
const LOOKUP_CELLS: usize = 64;

// The live rows that hold one of `ids` in an id column, ascending, found
// through the index each id column leads.
fn rows_holding(table: &mut Table, ids: &[u32]) -> Vec<u32> {
    let mut rows = Vec::new();
    for column in table.schema.ids.clone() {
        let Some(at) = table.id_index(column) else {
            continue;
        };
        table.catch_up_index(at);
        let index = &table.indexes[at];
        for &id in ids {
            let key = std::slice::from_ref(&id);
            rows.extend_from_slice(index.laid_rows(&table.columns, key));
            rows.extend(index.tail_rows(&table.columns, key));
        }
    }
    rows.retain(|&row| table.is_live(row));
    rows.sort_unstable();
    rows.dedup();
    rows
}

// Kills the live rows that hold a cell `map` moves, and returns them. The
// rows of each column mark a bitset, so the rows come out ascending and
// once each without a sort.
pub(crate) fn take_moved(table: &mut Table, map: &[u32], hits: &mut Vec<u32>) -> Delta {
    let mut marked = vec![0u64; table.len().div_ceil(64)];
    for &column in &table.schema.ids {
        hits.clear();
        select_moved(&table.columns[column], map, hits);
        for &row in hits.iter() {
            marked[row as usize / 64] |= 1 << (row % 64);
        }
    }
    and_words(&mut marked, table.live());
    let mut moved = Vec::new();
    for (word, &bits) in marked.iter().enumerate() {
        let mut bits = bits;
        while bits != 0 {
            moved.push((word * 64) as u32 + bits.trailing_zeros());
            bits &= bits - 1;
        }
    }
    take_rows(table, &moved)
}

// Kills the live `rows`, ascending, and returns them.
fn take_rows(table: &mut Table, moved: &[u32]) -> Delta {
    let gather = |column: &Vec<u32>| moved.iter().map(|&row| column[row as usize]).collect();
    let delta = Delta {
        columns: table.columns.iter().map(gather).collect(),
        subsumed: moved.iter().map(|&row| table.is_subsumed(row)).collect(),
    };
    let mut hashes = vec![0; moved.len()];
    let keys: Vec<&[u32]> = (delta.columns[..table.schema.keys].iter())
        .map(Vec::as_slice)
        .collect();
    hash_rows(&keys, &mut hashes);
    for (&row, &hash) in moved.iter().zip(&hashes) {
        table.remove(row, hash);
    }
    delta
}
