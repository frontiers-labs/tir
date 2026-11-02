// The batched interpreter. A batch holds one column per register, as many
// bindings as fit a quarter of the L2 cache; each step maps the batch of its
// parent to a new one.
// Matching reads committed rows only: actions buffer their writes in
// `Writes`, which apply after every rule of the iteration matched.

use std::collections::HashMap;

use tir_adt::FxBuildHasher;
use tir_adt::simd::{hash_rows, intersect_sorted, select_eq, select_same};

use tir_adt::simd::hash_row;

use super::Program;
use super::code::{Action, Bind, Node, Reg, Slice, Step};
use crate::core::{
    Colors, Delta, EMPTY, GroupRow, GroupView, RowTable, Table, UnionFind, check_pure, group_at,
    l2, same_key,
};
use crate::{Color, Database, Error, PrimCtx, TableId, TableKind, Value};

#[derive(Clone, Default)]
struct Batch {
    cols: Vec<Vec<u32>>,
    len: usize,
    // The color of each binding, in a color run.
    colors: Vec<u32>,
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
    removed_colors: Vec<(Color, TableId, Vec<Value>)>,
    subsumed: Vec<(usize, Vec<u32>)>,
    // The color of each union, in a color run.
    colors: Vec<u32>,
    // The groups FindOrMake made, by their color and rows, so equal rows
    // get one group per iteration; and the keyed rows of those groups, with
    // their color.
    made: HashMap<Vec<u32>, u32, FxBuildHasher>,
    keyed: Vec<(TableId, u32, u32)>,
}

struct Removal {
    table: TableId,
    row: u32,
    hash: u32,
}

impl Writes {
    // The rows of a color run lead with their color.
    fn new(program: &Program, colored: bool) -> Self {
        let file_tables = &program.file.tables;
        Self {
            rows: (file_tables.iter())
                .map(|table| Delta {
                    columns: vec![Vec::new(); usize::from(colored) + table.columns.len()],
                    subsumed: Vec::new(),
                })
                .collect(),
            entries: file_tables.iter().map(|_| RowTable::default()).collect(),
            unions: Vec::new(),
            removed: Vec::new(),
            removed_colors: Vec::new(),
            subsumed: Vec::new(),
            colors: Vec::new(),
            made: HashMap::default(),
            keyed: Vec::new(),
        }
    }

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
                db.remove_row(removal.table, removal.row, removal.hash);
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
            for (table, _, g) in writes.keyed {
                db.insert(table, &[Value::new(g)])?;
            }
            unions.extend(writes.unions);
        }
        for (a, b) in unions {
            db.union(Value::new(a), Value::new(b));
        }
        Ok(())
    }

    // Writes the rows and unions of a color run into their colors; a color
    // rebuild follows.
    pub(crate) fn apply_colors(
        writes: Vec<Self>,
        db: &mut Database,
        tables: &[TableId],
    ) -> Result<(), Error> {
        for writes in &writes {
            for (color, table, key) in &writes.removed_colors {
                db.remove_in(*color, *table, key);
            }
        }
        for writes in writes {
            for (delta, &table) in writes.rows.iter().zip(tables) {
                for (i, &subsumed) in delta.subsumed.iter().enumerate() {
                    let row = delta.columns.iter().map(|column| column[i]).collect();
                    db.write_in(table, row, subsumed)?;
                }
            }
            for (table, c, g) in writes.keyed {
                db.insert_in(Color::new(c), table, &[Value::new(g)])?;
            }
            for ((a, b), c) in writes.unions.into_iter().zip(writes.colors) {
                db.union_in(Color::new(c), Value::new(a), Value::new(b));
            }
        }
        Ok(())
    }
}

// How a search reads rows: the base path, or a color run in the copy form.
// The base path is monomorphized with no color work in it.
pub(crate) trait Reads<'a> {
    const COLORED: bool;

    fn copy(&self) -> &CopyRead<'a>;
}

pub(crate) struct BaseRead;

impl<'a> Reads<'a> for BaseRead {
    const COLORED: bool = false;

    fn copy(&self) -> &CopyRead<'a> {
        unreachable!("the base reads no color")
    }
}

// A color run in the copy form: each binding sees the base rows its color
// shows and the rows of its color. Base rows are never New here, so every
// match of a color run reads a row of its color and none repeats a base
// match.
pub(crate) struct CopyRead<'a> {
    colors: &'a Colors,
    // Rows of a color at or after its timestamp here are New, by color.
    last: Vec<u32>,
    // The mirror of each program index in its color table, color first.
    mirrors: Vec<usize>,
    // The (color) index of each color table, by base table.
    own: Vec<usize>,
}

impl<'a> Reads<'a> for CopyRead<'a> {
    const COLORED: bool = true;

    fn copy(&self) -> &CopyRead<'a> {
        self
    }
}

struct Exec<'a, R> {
    reads: R,
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
    let root = Batch {
        cols: program.consts[ruleset]
            .iter()
            .enumerate()
            .map(|(r, v)| {
                vec![if program.file.rulesets[ruleset]
                    .id_consts
                    .contains(&(r as u32))
                {
                    db.find(*v).raw()
                } else {
                    v.raw()
                }]
            })
            .collect(),
        len: 1,
        colors: Vec::new(),
    };
    let (ctx, indexes) = db.ctx_and_indexes();
    run(program, ctx, indexes, BaseRead, ruleset, last, &root)
}

// Matches one ruleset in each color of `level` at once, with the rows of a
// color from its entry in `last` on as New.
pub(crate) fn search_colors(
    program: &Program,
    db: &mut Database,
    ruleset: usize,
    level: &[Color],
    last: &[u32],
) -> Found {
    for &index in &program.used[ruleset] {
        let (table, at) = db.indexes[index.index()];
        db.tables[table.index()].refresh_index(at);
    }
    let mirrors = (program.indexes.iter())
        .map(|&index| {
            let (table, at) = db.indexes[index.index()];
            let order = &db.tables[table.index()].indexes[at].order;
            let order: Vec<usize> = std::iter::once(0)
                .chain(order.iter().map(|c| c + 1))
                .collect();
            db.colors.refresh(table, &order)
        })
        .collect();
    let own = (0..db.tables.len())
        .map(|table| db.colors.refresh(TableId::new(table), &[0]))
        .collect();
    db.refresh_group_mirrors();
    let n = level.len();
    let root = Batch {
        cols: (program.consts[ruleset].iter())
            .enumerate()
            .map(|(r, v)| {
                level
                    .iter()
                    .map(|&c| {
                        if program.file.rulesets[ruleset]
                            .id_consts
                            .contains(&(r as u32))
                        {
                            db.find_in(c, *v).raw()
                        } else {
                            v.raw()
                        }
                    })
                    .collect()
            })
            .collect(),
        len: n,
        colors: level.iter().map(|c| c.index() as u32).collect(),
    };
    let (ctx, indexes, colors) = db.parts();
    let mut by_color = vec![0; colors.len()];
    for (&c, &last) in level.iter().zip(last) {
        by_color[c.index()] = last;
    }
    let reads = CopyRead {
        colors,
        last: by_color,
        mirrors,
        own,
    };
    run(program, ctx, indexes, reads, ruleset, 0, &root)
}

fn run<'a, R: Reads<'a>>(
    program: &'a Program,
    ctx: PrimCtx<'a>,
    indexes: &'a [(TableId, usize)],
    reads: R,
    ruleset: usize,
    last: u32,
    root: &Batch,
) -> Found {
    let decl = &program.file.rulesets[ruleset];
    let mut exec = Exec {
        reads,
        ctx,
        indexes,
        program,
        last,
        writes: Writes::new(program, R::COLORED),
        matches: vec![0; decl.rules.len()],
        pool: Vec::new(),
        l2: l2(),
    };
    let mut result = (decl.roots.iter()).try_for_each(|&node| exec.node(node, root));
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

// Whether a base row is in the slice of a color run, where base rows are
// never New.
fn base_slice(table: &Table, row: u32, slice: Slice) -> bool {
    match slice {
        Slice::New => false,
        Slice::Any => true,
        Slice::Old | Slice::All => !table.is_subsumed(row),
    }
}

// Columns of the rows of one table to bind, and the slice they must be in.
struct Read<'r> {
    table: &'r Table,
    cols: &'r [usize],
    binds: &'r [Bind],
    slice: Slice,
    side: Side,
}

// Whose rows a read of a color run reads.
#[derive(Clone, Copy)]
enum Side {
    Base(TableId),
    Color,
}

// Where the value a `Bind::Same` column must hold comes from.
#[derive(Clone, Copy)]
enum Source {
    Reg(Reg),
    Column(usize),
}

impl<'a, R: Reads<'a>> Exec<'a, R> {
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

    fn index_table(&self, index: u32) -> TableId {
        self.indexes[self.program.indexes[index as usize].index()].0
    }

    fn table_id(&self, table: u32) -> TableId {
        self.program.tables[table as usize]
    }

    fn colors(&self) -> &'a Colors {
        self.reads.copy().colors
    }

    // Whether a color run sees `row` for a binding of color `c`.
    fn sees(&self, read: &Read<'_>, row: u32, c: u32) -> bool {
        match read.side {
            Side::Color => visible(
                read.table,
                row,
                read.slice,
                self.reads.copy().last[c as usize],
            ),
            Side::Base(id) => {
                base_slice(read.table, row, read.slice)
                    && self.colors().visible(c, id, read.table, row)
            }
        }
    }

    // The rows of the color table of `table` at index `at` whose index
    // columns hold the color of each binding, then `key`.
    fn color_ranges(
        &self,
        table: TableId,
        at: usize,
        key: &[Reg],
        input: &Batch,
    ) -> Vec<std::ops::Range<u32>> {
        let rows = self.colors().rows(table);
        let mut columns: Vec<&[u32]> = vec![&input.colors];
        columns.extend(key.iter().map(|&r| input.col(r)));
        let mut ranges = vec![0..0; input.len];
        rows.indexes[at].ranges(&rows.columns, &columns, &mut ranges);
        ranges
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
        let mut colors = Vec::new();
        if R::COLORED {
            colors = self.buffer();
            colors.extend(sel.iter().map(|&i| input.colors[i as usize]));
        }
        Batch {
            cols,
            len: sel.len(),
            colors,
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
            } if R::COLORED => return self.expand_colored(node, *index, key, outs, *slice, input),
            Step::Expand {
                index,
                key,
                outs,
                slice,
            } => {
                let (table, at) = self.index(*index);
                let side = Side::Base(self.index_table(*index));
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
                    side,
                };
                return self.emit(node, &read, input, found);
            }
            Step::Lookup {
                table,
                key,
                out,
                slice,
            } if R::COLORED => {
                self.lookup_colored(*table, key, *out, *slice, input, &mut sel, &mut added)?;
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
            Step::Check { index, key } if R::COLORED => {
                self.check_colored(*index, key, input, &mut sel)
            }
            Step::Check { index, key } => {
                let (table, at) = self.index(*index);
                let ranges = self.ranges(table, at, key, input);
                sel.extend((0..n as u32).filter(|&i| !ranges[i as usize].is_empty()));
            }
            Step::Same { a, b } => select_same(&input.col(*a)[..n], &input.col(*b)[..n], &mut sel),
            Step::Prim {
                prim,
                context,
                args,
                out,
            } => {
                self.ctx.context = *context;
                let prim = program.externs[*prim as usize];
                if R::COLORED {
                    check_pure(&self.ctx, prim)?;
                }
                let mut found = self.buffer();
                let mut values = Vec::with_capacity(args.len());
                for i in 0..n {
                    values.clear();
                    values.extend(args.iter().map(|&r| Value::new(input.col(r)[i])));
                    let result = if R::COLORED {
                        self.ctx
                            .call_in(self.reads.copy().colors, input.colors[i], prim, &values)
                    } else {
                        self.ctx.call(prim, &values)
                    };
                    let Some(value) = result else {
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
            Slice::New if R::COLORED => 0..0,
            _ if R::COLORED => 0..t.len(),
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
                    side: Side::Base(self.table_id(table)),
                };
                self.emit(node, &read, input, [(i, &live[..])].into_iter())?;
            }
        }
        if R::COLORED {
            let id = self.table_id(table);
            let at = self.reads.copy().own[id.index()];
            let ranges = self.color_ranges(id, at, &[], input);
            let rows = self.colors().rows(id);
            let all: Vec<usize> = (1..rows.columns.len()).collect();
            let read = Read {
                table: rows,
                cols: &all,
                binds: cols,
                slice,
                side: Side::Color,
            };
            let index = rows.indexes[at].rows();
            let found = (ranges.iter().enumerate())
                .map(|(i, range)| (i, &index[range.start as usize..range.end as usize]));
            self.emit(node, &read, input, found)?;
        }
        Ok(())
    }

    // Expand in a color run: the base rows the color shows, then the rows
    // of the color, through the mirror of the index.
    fn expand_colored(
        &mut self,
        node: &'a Node,
        index: u32,
        key: &[Reg],
        outs: &[Bind],
        slice: Slice,
        input: &Batch,
    ) -> Result<(), Error> {
        let (table, at) = self.index(index);
        let id = self.index_table(index);
        let order = &table.indexes[at].order;
        let cols: Vec<usize> = (0..table.columns.len())
            .filter(|c| !order.contains(c))
            .collect();
        if slice != Slice::New {
            let ranges = self.ranges(table, at, key, input);
            let rows = table.indexes[at].rows();
            let found = (ranges.iter().enumerate())
                .map(|(i, range)| (i, &rows[range.start as usize..range.end as usize]));
            let read = Read {
                table,
                cols: &cols,
                binds: outs,
                slice,
                side: Side::Base(id),
            };
            self.emit(node, &read, input, found)?;
        }
        let at = self.reads.copy().mirrors[index as usize];
        let ranges = self.color_ranges(id, at, key, input);
        let rows = self.colors().rows(id);
        let cols: Vec<usize> = cols.iter().map(|c| c + 1).collect();
        let index = rows.indexes[at].rows();
        let found = (ranges.iter().enumerate())
            .map(|(i, range)| (i, &index[range.start as usize..range.end as usize]));
        let read = Read {
            table: rows,
            cols: &cols,
            binds: outs,
            slice,
            side: Side::Color,
        };
        self.emit(node, &read, input, found)
    }

    // Check in a color run: a base row the color shows or a row of the color.
    fn check_colored(&mut self, index: u32, key: &[Reg], input: &Batch, sel: &mut Vec<u32>) {
        let (table, at) = self.index(index);
        let id = self.index_table(index);
        let ranges = self.ranges(table, at, key, input);
        let own = self.color_ranges(id, self.reads.copy().mirrors[index as usize], key, input);
        let rows = table.indexes[at].rows();
        let colors = self.colors();
        for (i, (range, own)) in ranges.iter().zip(&own).enumerate() {
            let c = input.colors[i];
            let shown = rows[range.start as usize..range.end as usize]
                .iter()
                .any(|&row| colors.visible(c, id, table, row));
            if shown || !own.is_empty() {
                sel.push(i as u32);
            }
        }
    }

    // Lookup in a color run: the row of the color with the key, else the
    // base row if the color shows it.
    #[expect(clippy::too_many_arguments, reason = "the operands of one step")]
    fn lookup_colored(
        &mut self,
        table: u32,
        key: &[Reg],
        out: Option<Bind>,
        slice: Slice,
        input: &Batch,
        sel: &mut Vec<u32>,
        added: &mut Vec<Vec<u32>>,
    ) -> Result<(), Error> {
        let (t, id) = (self.table(table), self.table_id(table));
        let colors = self.colors();
        let (rows, keys) = (colors.rows(id), t.schema.keys);
        let mut found = self.buffer();
        let mut cells = Vec::with_capacity(keys + 1);
        for i in 0..input.len {
            let c = input.colors[i];
            cells.clear();
            cells.push(c);
            cells.extend(key.iter().map(|&r| input.col(r)[i]));
            let hit = match colors.color_row(c, id, &cells) {
                Some(row) => visible(rows, row, slice, self.reads.copy().last[c as usize])
                    .then_some((rows, row, keys + 1)),
                None => t
                    .find(&cells[1..], hash_row(&cells[1..]))
                    .filter(|&row| base_slice(t, row, slice) && !colors.hides(c, id, t, row))
                    .map(|row| (t, row, keys)),
            };
            let Some((holder, row, output)) = hit else {
                if slice == Slice::Any {
                    let name = &t.schema.name;
                    return Err(Error::Panic(format!("`{name}` has no row for the key")));
                }
                continue;
            };
            match out {
                Some(Bind::New(_)) => found.push(holder.cell(row, output)),
                Some(Bind::Same(r)) if holder.cell(row, output) != input.col(r)[i] => continue,
                _ => {}
            }
            sel.push(i as u32);
        }
        if matches!(out, Some(Bind::New(_))) {
            added.push(found);
        } else {
            self.release(found);
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
                let seen = if R::COLORED {
                    self.sees(read, row, input.colors[i])
                } else {
                    visible(table, row, slice, self.last)
                };
                if !seen || !checks.iter().all(holds) {
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
        if R::COLORED {
            self.release(batch.colors);
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
            Action::Entry { table, args, .. } if R::COLORED => {
                let (t, id) = (self.table(*table), self.table_id(*table));
                let colors = self.colors();
                let (rows, keys) = (colors.rows(id), t.schema.keys);
                let mut out = self.buffer();
                let mut cells = Vec::with_capacity(keys + 1);
                for i in 0..n {
                    let c = batch.colors[i];
                    cells.clear();
                    cells.push(c);
                    cells.extend(args.iter().map(|&r| batch.col(r)[i]));
                    let base = || t.find(&cells[1..], hash_row(&cells[1..]));
                    out.push(match colors.color_row(c, id, &cells) {
                        Some(row) => rows.cell(row, keys + 1),
                        None => match base().filter(|&row| !colors.hides(c, id, t, row)) {
                            Some(row) => t.cell(row, keys),
                            None => {
                                let table = (*table as usize, t);
                                self.writes
                                    .entry(table, &cells, hash_row(&cells), self.ctx.uf)
                            }
                        },
                    });
                }
                batch.cols.push(out);
            }
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
                    let mut key = Vec::new();
                    let selected: Vec<usize> = (0..n)
                        .filter(|&i| {
                            if R::COLORED {
                                let c = batch.colors[i];
                                let id = self.table_id(*table);
                                key.clear();
                                key.push(c);
                                key.extend(args.iter().map(|&r| batch.col(r)[i]));
                                self.colors().color_row(c, id, &key).is_none()
                                    && (found[i] == EMPTY
                                        || !self.colors().visible(c, id, t, found[i]))
                            } else {
                                found[i] == EMPTY
                            }
                        })
                        .collect();
                    self.release(hashes);
                    self.release(found);
                    Some(selected)
                } else {
                    None
                };
                let delta = &mut self.writes.rows[*table as usize];
                let mut columns = delta.columns.iter_mut();
                if R::COLORED {
                    let colors = columns.next().expect("a color column");
                    match &selected {
                        Some(selected) => colors.extend(selected.iter().map(|&i| batch.colors[i])),
                        None => colors.extend(&batch.colors),
                    }
                }
                for (column, &r) in columns.zip(args) {
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
                if R::COLORED {
                    let colors = pairs.zip(&batch.colors).filter(|((a, b), _)| a != b);
                    for ((&a, &b), &c) in colors {
                        self.writes.unions.push((a, b));
                        self.writes.colors.push(c);
                    }
                    return Ok(());
                }
                let unions = pairs.filter(|(a, b)| a != b).map(|(&a, &b)| (a, b));
                self.writes.unions.extend(unions);
            }
            Action::Delete { table, args } if R::COLORED => {
                let (t, id) = (self.table(*table), self.table_id(*table));
                for i in 0..n {
                    let c = batch.colors[i];
                    for key in t.orders(&batch.row(args, i)) {
                        let mut cells = vec![c];
                        cells.extend_from_slice(&key);
                        let own = self.colors().color_row(c, id, &cells).is_some();
                        let base = t
                            .find(&key, hash_row(&key))
                            .is_some_and(|row| self.colors().visible(c, id, t, row));
                        if own || base {
                            self.writes.removed_colors.push((
                                Color::new(c),
                                id,
                                key.into_iter().map(Value::new).collect(),
                            ));
                        }
                    }
                }
            }
            Action::Subsume { .. } if R::COLORED => {
                return Err(Error::Unsupported(
                    "a rule that subsumes rows cannot run in a color".to_owned(),
                ));
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
            Action::Call {
                prim,
                context,
                args,
            } => {
                self.ctx.context = *context;
                let prim = self.program.externs[*prim as usize];
                if R::COLORED {
                    check_pure(&self.ctx, prim)?;
                }
                let mut values = Vec::with_capacity(args.len());
                for i in 0..n {
                    values.clear();
                    values.extend(args.iter().map(|&r| Value::new(batch.col(r)[i])));
                    let result = if R::COLORED {
                        self.ctx
                            .call_in(self.reads.copy().colors, batch.colors[i], prim, &values)
                    } else {
                        self.ctx.call(prim, &values)
                    };
                    if result.is_none() {
                        let name = &self.ctx.prims[prim.index()].name;
                        return Err(Error::Panic(format!("`{name}` has no result")));
                    }
                }
            }
            Action::Panic { message } => return Err(Error::Panic(message.clone())),
            Action::FindOrMake { rows, out } => {
                let found = self.find_or_make(rows, batch)?;
                debug_assert_eq!(*out as usize, batch.cols.len());
                batch.cols.push(found);
            }
        }
        Ok(())
    }

    // The group of each binding whose rows are `rows`: one found in the
    // view of its color, else one made earlier in the iteration, else a new
    // one whose rows the writes hold.
    fn find_or_make(&mut self, rows: &[(u32, Vec<Reg>)], batch: &Batch) -> Result<Vec<u32>, Error> {
        let tables: Vec<TableId> = rows.iter().map(|&(t, _)| self.table_id(t)).collect();
        let groups = self.ctx.groups;
        let Some(group) = group_at(groups, tables[0]).map(|at| &groups[at]) else {
            let name = &self.table(rows[0].0).schema.name;
            return Err(Error::Unsupported(format!(
                "`{name}` is no member table of a group"
            )));
        };
        let mut out = self.buffer();
        let mut spec = Vec::with_capacity(rows.len());
        for i in 0..batch.len {
            let c = if R::COLORED { batch.colors[i] } else { 0 };
            let view = GroupView {
                tables: self.ctx.tables,
                uf: self.ctx.uf,
                colors: R::COLORED.then(|| (self.colors(), c)),
            };
            spec.clear();
            for (&table, (file_table, regs)) in tables.iter().zip(rows) {
                let mut cells: Vec<u32> = regs.iter().map(|&r| batch.col(r)[i]).collect();
                view.canonical(table, &mut cells);
                spec.push((table, cells, *file_table));
            }
            spec.sort_unstable();
            spec.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
            let sorted: Vec<GroupRow> = spec
                .iter()
                .map(|(table, cells, _)| (*table, cells.clone()))
                .collect();
            let mut key = vec![c];
            for (table, cells) in &sorted {
                key.push(table.index() as u32);
                key.extend(cells);
            }
            if let Some(g) = self
                .writes
                .made
                .get(&key)
                .copied()
                .or_else(|| view.find(group, &sorted, EMPTY))
            {
                out.push(g);
                continue;
            }
            let g = self.ctx.uf.fresh();
            for (_, cells, t) in &spec {
                let delta = &mut self.writes.rows[*t as usize];
                let colored = R::COLORED.then_some(c);
                let row = colored.into_iter().chain([g]).chain(cells.iter().copied());
                for (column, cell) in delta.columns.iter_mut().zip(row) {
                    column.push(cell);
                }
                delta.subsumed.push(false);
            }
            self.writes.keyed.push((group.keyed, c, g));
            self.writes.made.insert(key, g);
            out.push(g);
        }
        Ok(out)
    }
}
