// The batched interpreter. A batch holds one column per register, as many
// bindings as fit a quarter of the L2 cache; each step maps the batch of its
// parent to a new one.
// Matching reads committed rows only: actions buffer their writes in
// `Writes`, which apply after every rule of the iteration matched.

use tir_adt::simd::{hash_row, hash_rows, intersect_sorted, select_eq, select_same};

use super::Program;
use super::code::{Action, Bind, Node, Reg, Slice, Step};
use crate::core::{Delta, EMPTY, RowTable, Table, UnionFind, l2, same_key};
use crate::{Database, Error, PrimCtx, TableId, TableKind, Value};

#[derive(Clone, Default)]
struct Batch {
    cols: Vec<Vec<u32>>,
    len: usize,
}

impl Batch {
    fn col(&self, reg: Reg) -> &[u32] {
        &self.cols[reg as usize]
    }

    fn row(&self, regs: &[Reg], i: usize) -> Vec<u32> {
        regs.iter().map(|&r| self.cols[r as usize][i]).collect()
    }
}

// The writes of one iteration, per table of the program.
pub(crate) struct Writes {
    rows: Vec<Delta>,
    // The rows Entry made, by key, so a key gets one e-class per iteration.
    entries: Vec<RowTable>,
    unions: Vec<(u32, u32)>,
    removed: Vec<Removal>,
    subsumed: Vec<(usize, Vec<u32>)>,
}

struct Removal {
    table: TableId,
    row: u32,
    hash: u32,
}

impl Writes {
    // A function with a default makes its default rather than a fresh id.
    fn entry(&mut self, table: (usize, &Table), key: &[u32], hash: u32, uf: &mut UnionFind) -> u32 {
        let delta = &mut self.rows[table.0];
        let entries = &mut self.entries[table.0];
        if let Some(row) = entries.get(hash, |row| same_key(&delta.columns, row, key)) {
            return delta.columns[key.len()][row as usize];
        }
        let default = table.1.schema.default_value();
        let id = default.map_or_else(|| uf.fresh(), Value::raw);
        let row = delta.subsumed.len() as u32;
        for (column, &cell) in delta.columns.iter_mut().zip(key.iter().chain([&id])) {
            column.push(cell);
        }
        delta.subsumed.push(false);
        entries.insert(hash, row, |_| false);
        id
    }

    // Searches have finished: row ids remain stable until every removal is done.
    pub(crate) fn apply(
        writes: Vec<Self>,
        db: &mut Database,
        tables: &[TableId],
    ) -> Result<(), Error> {
        for writes in &writes {
            for removal in &writes.removed {
                let table = &mut db.tables[removal.table.index()];
                if table.is_live(removal.row) {
                    table.remove(removal.row, removal.hash);
                }
            }
        }
        let mut unions = Vec::new();
        for writes in writes {
            for (mut delta, &table) in writes.rows.into_iter().zip(tables) {
                if !delta.subsumed.is_empty() {
                    db.apply(table, &mut delta)?;
                }
            }
            for (table, key) in writes.subsumed {
                let key: Vec<Value> = key.into_iter().map(Value::new).collect();
                db.subsume(tables[table], &key);
            }
            unions.extend(writes.unions);
        }
        for (a, b) in unions {
            db.union(Value::new(a), Value::new(b));
        }
        Ok(())
    }
}

struct Exec<'a> {
    ctx: PrimCtx<'a>,
    indexes: &'a [(TableId, usize)],
    program: &'a Program,
    // Rows written at this timestamp or later are New.
    last: u32,
    writes: Writes,
    matches: Vec<usize>,
    // Empty column buffers of finished batches, for new ones.
    pool: Vec<Vec<u32>>,
    l2: usize,
}

// The writes of a search, the matches of each rule, and the error that
// stopped the search early, if any.
pub(crate) type Found = (Writes, Vec<usize>, Result<(), Error>);

// Matches one ruleset, with rows from `last` on as New.
pub(crate) fn search(program: &Program, db: &mut Database, ruleset: usize, last: u32) -> Found {
    for &index in &program.used[ruleset] {
        let (table, at) = db.indexes[index.index()];
        db.tables[table.index()].refresh_index(at);
    }
    let decl = &program.file.rulesets[ruleset];
    let file_tables = &program.file.tables;
    let writes = Writes {
        rows: (file_tables.iter())
            .map(|table| Delta {
                columns: vec![Vec::new(); table.columns.len()],
                subsumed: Vec::new(),
            })
            .collect(),
        entries: file_tables.iter().map(|_| RowTable::default()).collect(),
        unions: Vec::new(),
        removed: Vec::new(),
        subsumed: Vec::new(),
    };
    let (ctx, indexes) = db.ctx_and_indexes();
    let mut exec = Exec {
        ctx,
        indexes,
        program,
        last,
        writes,
        matches: vec![0; decl.rules.len()],
        pool: Vec::new(),
        l2: l2(),
    };
    let root = Batch {
        cols: program.consts[ruleset]
            .iter()
            .map(|v| vec![v.raw()])
            .collect(),
        len: 1,
    };
    let mut result = (decl.roots.iter()).try_for_each(|&node| exec.node(node, &root));
    if result.is_ok() && exec.ctx.panicked.get() {
        result = Err(Error::Panic("a primitive panicked".to_owned()));
    }
    (exec.writes, exec.matches, result)
}

// Whether a row is in the slice.
fn visible(table: &Table, row: u32, slice: Slice, last: u32) -> bool {
    match slice {
        Slice::Any => true,
        Slice::All => !table.is_subsumed(row),
        Slice::New => !table.is_subsumed(row) && table.timestamp(row) >= last,
        Slice::Old => !table.is_subsumed(row) && table.timestamp(row) < last,
    }
}

// Columns of the rows of one table to bind, and the slice they must be in.
struct Read<'r> {
    table: &'r Table,
    cols: &'r [usize],
    binds: &'r [Bind],
    slice: Slice,
}

// Where the value a `Bind::Same` column must hold comes from.
#[derive(Clone, Copy)]
enum Source {
    Reg(Reg),
    Column(usize),
}

impl<'a> Exec<'a> {
    fn table(&self, table: u32) -> &'a Table {
        let tables: &'a [Table] = self.ctx.tables;
        &tables[self.program.tables[table as usize].index()]
    }

    // The table and the index of a program index.
    fn index(&self, index: u32) -> (&'a Table, usize) {
        let tables: &'a [Table] = self.ctx.tables;
        let (table, at) = self.indexes[self.program.indexes[index as usize].index()];
        (&tables[table.index()], at)
    }

    // Bindings of `bytes` each that fit a quarter of the L2 cache, so a
    // batch, the batch it comes from and the rows they read stay in L2. The
    // cost of a step is mostly per batch below that; past it, batches
    // stream from L3.
    fn batch_len(&self, bytes: usize) -> usize {
        (self.l2 / 4 / bytes).max(1)
    }

    fn buffer(&mut self) -> Vec<u32> {
        self.pool.pop().unwrap_or_default()
    }

    fn release(&mut self, mut buffer: Vec<u32>) {
        buffer.clear();
        self.pool.push(buffer);
    }

    // The bindings of `input` at `sel`, then the columns `added`.
    fn select(&mut self, input: &Batch, sel: &[u32], added: Vec<Vec<u32>>) -> Batch {
        let mut cols = Vec::with_capacity(input.cols.len() + added.len());
        for col in &input.cols {
            let mut out = self.buffer();
            out.extend(sel.iter().map(|&i| col[i as usize]));
            cols.push(out);
        }
        cols.extend(added);
        Batch {
            cols,
            len: sel.len(),
        }
    }

    fn node(&mut self, id: u32, input: &Batch) -> Result<(), Error> {
        if self.ctx.stop.get() {
            return Ok(());
        }
        let program: &'a Program = self.program;
        let node = &program.file.nodes[id as usize];
        let n = input.len;
        let mut sel = self.buffer();
        let mut added: Vec<Vec<u32>> = Vec::new();
        match &node.step {
            Step::Scan { table, slice, cols } => {
                return self.scan(node, *table, *slice, cols, input);
            }
            Step::Expand {
                index,
                key,
                outs,
                slice,
            } => {
                let (table, at) = self.index(*index);
                let index = &table.indexes[at];
                let ranges = self.ranges(table, at, key, input);
                let rows = index.rows();
                let cols: Vec<usize> = (0..table.columns.len())
                    .filter(|c| !index.order.contains(c))
                    .collect();
                let found = (ranges.iter().enumerate())
                    .map(|(i, range)| (i, &rows[range.start as usize..range.end as usize]));
                let read = Read {
                    table,
                    cols: &cols,
                    binds: outs,
                    slice: *slice,
                };
                return self.emit(node, &read, input, found);
            }
            Step::Lookup {
                table,
                key,
                out,
                slice,
            } => {
                let t = self.table(*table);
                let keys = t.schema.keys;
                let columns: Vec<&[u32]> = key.iter().map(|&r| input.col(r)).collect();
                let (mut hashes, mut rows) = (self.buffer(), self.buffer());
                hashes.resize(n, 0);
                rows.resize(n, EMPTY);
                hash_rows(&columns, &mut hashes);
                t.find_many(&columns, &hashes, &mut rows);
                let mut found = self.buffer();
                for (i, &row) in rows.iter().enumerate() {
                    if row == EMPTY || !visible(t, row, *slice, self.last) {
                        if *slice == Slice::Any {
                            let name = &t.schema.name;
                            return Err(Error::Panic(format!("`{name}` has no row for the key")));
                        }
                        continue;
                    }
                    match *out {
                        Some(Bind::New(_)) => found.push(t.cell(row, keys)),
                        Some(Bind::Same(r)) if t.cell(row, keys) != input.col(r)[i] => continue,
                        _ => {}
                    }
                    sel.push(i as u32);
                }
                self.release(hashes);
                self.release(rows);
                if matches!(out, Some(Bind::New(_))) {
                    added.push(found);
                }
            }
            Step::Check { index, key } => {
                let (table, at) = self.index(*index);
                let ranges = self.ranges(table, at, key, input);
                sel.extend((0..n as u32).filter(|&i| !ranges[i as usize].is_empty()));
            }
            Step::Same { a, b } => select_same(&input.col(*a)[..n], &input.col(*b)[..n], &mut sel),
            Step::Prim { prim, args, out } => {
                let prim = program.externs[*prim as usize];
                let mut found = self.buffer();
                let mut values = Vec::with_capacity(args.len());
                for i in 0..n {
                    values.clear();
                    values.extend(args.iter().map(|&r| Value::new(input.col(r)[i])));
                    let Some(value) = self.ctx.call(prim, &values) else {
                        continue;
                    };
                    match *out {
                        Bind::New(_) => found.push(value.raw()),
                        Bind::Same(r) if value.raw() != input.col(r)[i] => continue,
                        Bind::Same(_) => {}
                    }
                    sel.push(i as u32);
                }
                if matches!(out, Bind::New(_)) {
                    added.push(found);
                }
            }
        }
        let batch = self.select(input, &sel, added);
        self.release(sel);
        self.finish(node, batch)
    }

    fn ranges(
        &self,
        table: &Table,
        at: usize,
        key: &[Reg],
        input: &Batch,
    ) -> Vec<std::ops::Range<u32>> {
        let columns: Vec<&[u32]> = key.iter().map(|&r| input.col(r)).collect();
        let mut ranges = vec![0..0; input.len];
        table.indexes[at].ranges(&table.columns, &columns, &mut ranges);
        ranges
    }

    // Scans the rows of the slice in chunks for each binding. A column that
    // must equal a register or another column is filtered a chunk at a time.
    fn scan(
        &mut self,
        node: &'a Node,
        table: u32,
        slice: Slice,
        cols: &[Bind],
        input: &Batch,
    ) -> Result<(), Error> {
        let t = self.table(table);
        let start = t.since(self.last).start as usize;
        let rows = match slice {
            Slice::New => start..t.len(),
            Slice::Old => 0..start,
            Slice::All | Slice::Any => 0..t.len(),
        };
        let width = input.cols.len() as u32;
        let column = |c: usize, rows: &std::ops::Range<usize>| &t.columns[c][rows.clone()];
        // A chunk of rows: its cells in each column, and the rows it keeps.
        let size = self.batch_len(4 * (cols.len() + 1));
        let all: Vec<usize> = (0..cols.len()).collect();
        for i in 0..input.len {
            for at in rows.clone().step_by(size) {
                let chunk = at..(at + size).min(rows.end);
                let mut hits: Option<Vec<u32>> = None;
                for (c, bind) in cols.iter().enumerate() {
                    let Bind::Same(r) = *bind else { continue };
                    let mut found = Vec::new();
                    if r < width {
                        select_eq(column(c, &chunk), input.col(r)[i], &mut found);
                    } else {
                        let other = cols.iter().position(|&b| b == Bind::New(r));
                        let other = other.expect("a register is bound before it is read");
                        select_same(column(c, &chunk), column(other, &chunk), &mut found);
                    }
                    hits = Some(match hits {
                        None => found,
                        Some(hits) => {
                            let mut both = Vec::new();
                            intersect_sorted(&hits, &found, &mut both);
                            both
                        }
                    });
                }
                let offsets = hits.unwrap_or_else(|| (0..chunk.len() as u32).collect());
                let live: Vec<u32> = (offsets.into_iter())
                    .map(|o| (chunk.start + o as usize) as u32)
                    .filter(|&row| t.is_live(row))
                    .collect();
                let read = Read {
                    table: t,
                    cols: &all,
                    binds: cols,
                    slice,
                };
                self.emit(node, &read, input, [(i, &live[..])].into_iter())?;
            }
        }
        Ok(())
    }

    // Binds the columns `cols` of the rows `found`, each group of rows
    // paired with the binding it extends, and runs the node on each batch
    // of them.
    fn emit<'r>(
        &mut self,
        node: &'a Node,
        read: &Read<'_>,
        input: &Batch,
        found: impl Iterator<Item = (usize, &'r [u32])>,
    ) -> Result<(), Error> {
        let (table, slice) = (read.table, read.slice);
        let width = input.cols.len() as u32;
        let (mut news, mut checks) = (Vec::new(), Vec::new());
        for (&c, &bind) in read.cols.iter().zip(read.binds) {
            match bind {
                Bind::New(_) => news.push(c),
                Bind::Same(r) if r < width => checks.push((c, Source::Reg(r))),
                Bind::Same(r) => checks.push((c, Source::Column(news[(r - width) as usize]))),
            }
        }
        let size = self.batch_len(4 * (input.cols.len() + news.len() + 1));
        let mut sel = self.buffer();
        let mut added: Vec<Vec<u32>> = news.iter().map(|_| self.buffer()).collect();
        for (i, rows) in found {
            for &row in rows {
                let holds = |&(c, source): &(usize, Source)| {
                    let want = match source {
                        Source::Reg(r) => input.col(r)[i],
                        Source::Column(other) => table.cell(row, other),
                    };
                    table.cell(row, c) == want
                };
                if !visible(table, row, slice, self.last) || !checks.iter().all(holds) {
                    continue;
                }
                sel.push(i as u32);
                for (column, &c) in added.iter_mut().zip(&news) {
                    column.push(table.cell(row, c));
                }
                if sel.len() == size {
                    let fresh = news.iter().map(|_| self.buffer()).collect();
                    let batch = self.select(input, &sel, std::mem::replace(&mut added, fresh));
                    self.finish(node, batch)?;
                    sel.clear();
                }
            }
        }
        let batch = self.select(input, &sel, added);
        self.release(sel);
        self.finish(node, batch)
    }

    // Runs the actions and children of `node` on `batch`, then keeps its
    // buffers for later batches.
    fn finish(&mut self, node: &'a Node, mut batch: Batch) -> Result<(), Error> {
        let result = self.run_node(node, &mut batch);
        for col in batch.cols {
            self.release(col);
        }
        result
    }

    fn run_node(&mut self, node: &'a Node, batch: &mut Batch) -> Result<(), Error> {
        if batch.len == 0 {
            return Ok(());
        }
        for &rule in &node.rules {
            self.matches[rule as usize] += batch.len;
        }
        for action in &node.actions {
            self.action(action, batch)?;
        }
        for &child in &node.children {
            self.node(child, batch)?;
        }
        Ok(())
    }

    fn action(&mut self, action: &Action, batch: &mut Batch) -> Result<(), Error> {
        let n = batch.len;
        match action {
            Action::Entry { table, args, .. } => {
                let t = self.table(*table);
                let (mut hashes, mut out) = (self.buffer(), self.buffer());
                hashes.resize(n, 0);
                out.resize(n, EMPTY);
                let columns: Vec<&[u32]> = args.iter().map(|&r| batch.col(r)).collect();
                hash_rows(&columns, &mut hashes);
                t.find_many(&columns, &hashes, &mut out);
                let mut key = Vec::with_capacity(args.len());
                for (i, (cell, &hash)) in out.iter_mut().zip(&hashes).enumerate() {
                    *cell = if *cell == EMPTY {
                        key.clear();
                        key.extend(columns.iter().map(|column| column[i]));
                        let table = (*table as usize, t);
                        self.writes.entry(table, &key, hash, self.ctx.uf)
                    } else {
                        t.cell(*cell, args.len())
                    };
                }
                self.release(hashes);
                batch.cols.push(out);
            }
            Action::Set { table, args } => {
                let t = self.table(*table);
                let selected = if t.schema.kind() == TableKind::Relation {
                    let (mut hashes, mut found) = (self.buffer(), self.buffer());
                    hashes.resize(n, 0);
                    found.resize(n, EMPTY);
                    let columns: Vec<&[u32]> = args.iter().map(|&r| batch.col(r)).collect();
                    hash_rows(&columns, &mut hashes);
                    t.find_many(&columns, &hashes, &mut found);
                    let selected: Vec<usize> = (0..n).filter(|&i| found[i] == EMPTY).collect();
                    self.release(hashes);
                    self.release(found);
                    Some(selected)
                } else {
                    None
                };
                let delta = &mut self.writes.rows[*table as usize];
                for (column, &r) in delta.columns.iter_mut().zip(args) {
                    match &selected {
                        Some(selected) => column.extend(selected.iter().map(|&i| batch.col(r)[i])),
                        None => column.extend_from_slice(batch.col(r)),
                    }
                }
                let added = selected.as_ref().map_or(n, Vec::len);
                delta.subsumed.resize(delta.subsumed.len() + added, false);
            }
            Action::Union { a, b } => {
                let pairs = batch.col(*a).iter().zip(batch.col(*b));
                let unions = pairs.filter(|(a, b)| a != b).map(|(&a, &b)| (a, b));
                self.writes.unions.extend(unions);
            }
            Action::Delete { table, args } => {
                let t = self.table(*table);
                for i in 0..n {
                    for key in t.orders(&batch.row(args, i)) {
                        let hash = hash_row(&key);
                        if let Some(row) = t.find(&key, hash) {
                            self.writes.removed.push(Removal {
                                table: self.program.tables[*table as usize],
                                row,
                                hash,
                            });
                        }
                    }
                }
            }
            Action::Subsume { table, args } => {
                let keys = (0..n).map(|i| (*table as usize, batch.row(args, i)));
                self.writes.subsumed.extend(keys);
            }
            Action::Call { prim, args } => {
                let prim = self.program.externs[*prim as usize];
                let mut values = Vec::with_capacity(args.len());
                for i in 0..n {
                    values.clear();
                    values.extend(args.iter().map(|&r| Value::new(batch.col(r)[i])));
                    if self.ctx.call(prim, &values).is_none() {
                        let name = &self.ctx.prims[prim.index()].name;
                        return Err(Error::Panic(format!("`{name}` has no result")));
                    }
                }
            }
            Action::Panic { message } => return Err(Error::Panic(message.clone())),
        }
        Ok(())
    }
}
