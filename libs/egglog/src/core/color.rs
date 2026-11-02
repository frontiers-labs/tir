use std::collections::HashMap;
use std::iter::once;

use tir_adt::FxBuildHasher;
use tir_adt::simd::hash_row;

use super::database::{PrimCtx, Row};
use super::rebuild::take_moved;
use super::table::Table;
use super::{Database, Error, Merge, Shape, SortId, TableId, TableKind, TableSchema, Value};
use crate::Context;

/// A set of facts that holds above the base and above other colors.
///
/// The view of a color is the fixed point of the rules over the base facts
/// and the facts of every color at or below it, as one e-graph. A base fact
/// holds in every color, also when it arrives after the color was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Color(u32);

impl Color {
    /// The base: below every color.
    pub const BASE: Color = Color(0);

    pub(crate) fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

// The colors of a database. Every color keeps its own copy of what its
// parents hold, so what a color sees is the base and its own rows: its class
// map, and its rows of each table, keyed by the color first. Each class map
// entry maps a base root to its root in the color, the least id of its
// class; a class root has none. Per-color vectors start with a slot for
// BASE once the first color is made.
#[derive(Clone)]
pub(crate) struct Colors {
    // Covering parents, by color.
    below_start: Vec<u32>,
    below: Vec<Color>,
    // 1 + the highest level of a parent; BASE is 0.
    level: Vec<u32>,
    live: Vec<u64>,
    // The color timestamp at which a color last read new base rows; a search
    // of a ruleset older than it reads every row of the color as New.
    rebased: Vec<u32>,
    // The base timestamp a color caught up with.
    last_base: Vec<u32>,
    // The color timestamp up to which a color imported its parents' rows.
    imported: Vec<u32>,
    // The timestamp of the rows of `classes` and `rows`.
    now: u32,
    // (color, base root) -> root in the color.
    classes: Table,
    // Per base table: (color, base key...) -> output.
    rows: Vec<Table>,
    // The live rows of each color in each table of `rows`, and its entries
    // in `classes`, so a color with none skips the probe.
    held: Vec<Vec<u32>>,
    entries: Vec<u32>,
    // (color, a, b) for the next color rebuild.
    unions: Vec<(u32, u32, u32)>,
}

// The color column is no value of a sort; the i64 sort of every database
// fills its slot.
const COLOR_SORT: SortId = SortId::first();

impl Colors {
    pub(crate) fn new() -> Self {
        let schema = TableSchema {
            name: "@classes".to_owned(),
            kind: TableKind::Function,
            columns: vec![COLOR_SORT; 3],
            keys: 2,
            shapes: vec![Shape::Dense; 3],
            merge: None,
            ids: vec![1, 2],
            symmetries: Vec::new(),
            cost: None,
            unextractable: true,
            default: None,
        };
        Self {
            below_start: Vec::new(),
            below: Vec::new(),
            level: Vec::new(),
            live: Vec::new(),
            rebased: Vec::new(),
            last_base: Vec::new(),
            imported: Vec::new(),
            now: 0,
            classes: Table::new(schema),
            rows: Vec::new(),
            held: Vec::new(),
            entries: Vec::new(),
            unions: Vec::new(),
        }
    }

    // Whether no color is live.
    pub(crate) fn is_empty(&self) -> bool {
        self.live.iter().all(|&word| word == 0)
    }

    fn is_live(&self, c: u32) -> bool {
        (self.live.get(c as usize / 64)).is_some_and(|word| word >> (c % 64) & 1 != 0)
    }

    // Colors made, dropped ones and BASE included.
    pub(crate) fn len(&self) -> usize {
        self.level.len()
    }

    pub(crate) fn timestamp(&self) -> u32 {
        self.now
    }

    pub(crate) fn rebased(&self, c: Color) -> u32 {
        self.rebased[c.index()]
    }

    // Grows with every change to the rows of a color.
    pub(crate) fn changes(&self) -> u64 {
        self.classes.version() + self.rows.iter().map(Table::version).sum::<u64>()
    }

    pub(crate) fn rows(&self, table: TableId) -> &Table {
        &self.rows[table.index()]
    }

    // The live colors by level, lowest first.
    pub(crate) fn levels(&self) -> Vec<Vec<Color>> {
        let mut levels: Vec<Vec<Color>> = Vec::new();
        for c in 1..self.len() as u32 {
            if self.is_live(c) {
                let level = self.level[c as usize] as usize;
                levels.resize_with(levels.len().max(level), Vec::new);
                levels[level - 1].push(Color(c));
            }
        }
        levels.retain(|level| !level.is_empty());
        levels
    }

    fn parents(&self, c: u32) -> &[Color] {
        let c = c as usize;
        &self.below[self.below_start[c] as usize..self.below_start[c + 1] as usize]
    }

    // The root in `c` of the base root `x`.
    pub(crate) fn root(&self, c: u32, x: u32) -> u32 {
        if self.entries.get(c as usize).is_none_or(|&n| n == 0) {
            return x;
        }
        let key = [c, x];
        let row = self.classes.find(&key, hash_row(&key));
        row.map_or(x, |row| self.classes.cell(row, 2))
    }

    // Whether a cell of the base row `row` is no root in `c`.
    pub(crate) fn holds_moved(&self, c: u32, table: &Table, row: u32) -> bool {
        self.entries[c as usize] > 0
            && (table.schema.ids.iter()).any(|&column| {
                let key = [c, table.cell(row, column)];
                self.classes.find(&key, hash_row(&key)).is_some()
            })
    }

    // The row of `c` with `key`, the color first.
    pub(crate) fn color_row(&self, c: u32, table: TableId, key: &[u32]) -> Option<u32> {
        let held = self.held.get(table.index())?.get(c as usize)?;
        if *held == 0 {
            return None;
        }
        self.rows[table.index()].find(key, hash_row(key))
    }

    // Whether `c` shows the base row `row` of `table`: none of its ids moved
    // in `c`, and `c` has no row of its key.
    pub(crate) fn visible(&self, c: u32, id: TableId, table: &Table, row: u32) -> bool {
        if self.holds_moved(c, table, row) {
            return false;
        }
        let held = self
            .held
            .get(id.index())
            .and_then(|held| held.get(c as usize));
        if held.is_none_or(|&n| n == 0) {
            return true;
        }
        let key: Vec<u32> = once(c)
            .chain((0..table.schema.keys).map(|column| table.cell(row, column)))
            .collect();
        self.color_row(c, id, &key).is_none()
    }

    // A color table for each base table, and a count per color for each.
    fn ensure(&mut self, tables: &[Table]) {
        for table in &tables[self.rows.len()..] {
            self.rows.push(Table::new(colored(&table.schema)));
            self.held.push(Vec::new());
        }
        for (rows, table) in self.rows.iter_mut().zip(tables) {
            if rows.schema.symmetries.len() != table.schema.symmetries.len() {
                rows.schema.symmetries = colored(&table.schema).symmetries;
            }
        }
        for held in &mut self.held {
            held.resize(self.level.len(), 0);
        }
    }

    fn make(&mut self, below: &[Color], base_now: u32) -> Color {
        if self.level.is_empty() {
            self.below_start.extend([0, 0]);
            self.level.push(0);
            self.rebased.push(0);
            self.last_base.push(0);
            self.imported.push(0);
            self.entries.push(0);
        }
        let mut parents: Vec<Color> = (below.iter().copied())
            .filter(|&parent| parent != Color::BASE)
            .collect();
        parents.sort_unstable();
        parents.dedup();
        assert!(
            parents.iter().all(|parent| self.is_live(parent.0)),
            "a color above a dropped color"
        );
        let id = u32::try_from(self.len()).expect("colors fit in u32");
        let level = parents.iter().map(|p| self.level[p.index()]).max();
        self.level.push(1 + level.unwrap_or(0));
        self.below.extend(parents);
        self.below_start.push(self.below.len() as u32);
        self.live
            .resize(self.live.len().max(id as usize / 64 + 1), 0);
        self.live[id as usize / 64] |= 1 << (id % 64);
        self.rebased.push(0);
        self.last_base.push(base_now);
        self.imported.push(0);
        self.entries.push(0);
        Color(id)
    }

    // The index of the color table of `table` by `order`, built.
    pub(crate) fn refresh(&mut self, table: TableId, order: &[usize]) -> usize {
        let rows = &mut self.rows[table.index()];
        let at = rows.add_index(order);
        rows.refresh_index(at);
        at
    }

    // The rows of `c` in `table` written at `since` or later, by the
    // (color) index.
    fn rows_of(table: &mut Table, c: u32, since: u32) -> Vec<(Vec<u32>, bool)> {
        let at = table.add_index(&[0]);
        table.refresh_index(at);
        let index = &table.indexes[at];
        let mut range = 0..0;
        index.ranges(&table.columns, &[&[c]], std::slice::from_mut(&mut range));
        let rows = &index.rows()[range.start as usize..range.end as usize];
        (rows.iter().copied())
            .filter(|&row| table.timestamp(row) >= since)
            .map(|row| (cells(table, row), table.is_subsumed(row)))
            .collect()
    }

    fn remove(table: &mut Table, row: u32) {
        let key: Vec<u32> = (0..table.schema.keys).map(|c| table.cell(row, c)).collect();
        table.remove(row, hash_row(&key));
    }
}

// The schema of the color rows of a base table: a color column first.
fn colored(base: &TableSchema) -> TableSchema {
    let shift = |columns: &[usize]| -> Vec<usize> { columns.iter().map(|c| c + 1).collect() };
    TableSchema {
        name: base.name.clone(),
        kind: base.kind,
        columns: once(COLOR_SORT)
            .chain(base.columns.iter().copied())
            .collect(),
        keys: base.keys + 1,
        shapes: once(Shape::Dense)
            .chain(base.shapes.iter().copied())
            .collect(),
        merge: base.merge.clone(),
        ids: shift(&base.ids),
        symmetries: (base.symmetries.iter())
            .map(|order| once(0).chain(shift(order)).collect())
            .collect(),
        cost: base.cost,
        unextractable: base.unextractable,
        default: base.default,
    }
}

fn cells(table: &Table, row: u32) -> Vec<u32> {
    (0..table.columns.len())
        .map(|c| table.cell(row, c))
        .collect()
}

fn is_pure(ctx: &PrimCtx<'_>, prim: super::ExternId) -> bool {
    ctx.prims[prim.index()].contexts.contains(&Context::Pure)
}

// Whether a color run may call `prim`: only a primitive that reads and
// writes no table, as it would read or write the base.
pub(crate) fn check_pure(ctx: &PrimCtx<'_>, prim: super::ExternId) -> Result<(), Error> {
    if is_pure(ctx, prim) {
        return Ok(());
    }
    let name = &ctx.prims[prim.index()].name;
    Err(Error::Unsupported(format!(
        "`{name}` may read or write tables, which a color run cannot: only pure primitives run in a color"
    )))
}

// The merge of `old` and `new` in color `c`: a union is a union in `c`.
fn eval_in(
    merge: &Merge,
    ctx: &mut PrimCtx<'_>,
    unions: &mut Vec<(u32, u32, u32)>,
    c: u32,
    old: Value,
    new: Value,
) -> Result<Option<Value>, Error> {
    Ok(match merge {
        Merge::AssertEq => (old == new).then_some(old),
        // Both are roots in `c`, and the least id of a class is its root.
        Merge::Union => {
            unions.push((c, old.raw(), new.raw()));
            Some(old.min(new))
        }
        Merge::Old => Some(old),
        Merge::New => Some(new),
        Merge::Const(value) => Some(*value),
        Merge::Prim(prim, args) => {
            check_pure(ctx, *prim)?;
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                let Some(value) = eval_in(arg, ctx, unions, c, old, new)? else {
                    return Ok(None);
                };
                values.push(value);
            }
            ctx.call(*prim, &values)
        }
        Merge::Function(..) => {
            return Err(Error::Unsupported(
                "a merge that looks up a table cannot run in a color".to_owned(),
            ));
        }
    })
}

impl Database {
    /// A new color above every color of `below`; above the base alone when
    /// `below` is empty. It holds what its parents hold from the next
    /// [`Database::rebuild`] or run on.
    ///
    /// # Panics
    ///
    /// If a color of `below` was dropped.
    pub fn color(&mut self, below: &[Color]) -> Color {
        let color = self.colors.make(below, self.now);
        self.colors.ensure(&self.tables);
        color
    }

    /// One new color above each color of `below`.
    pub fn colors(&mut self, below: &[Color]) -> Vec<Color> {
        below.iter().map(|&parent| self.color(&[parent])).collect()
    }

    /// Drop `color` and every color above it, with their facts.
    ///
    /// # Panics
    ///
    /// If `color` is the base.
    pub fn drop_color(&mut self, color: Color) {
        assert_ne!(color, Color::BASE, "the base is no color to drop");
        let colors = &mut self.colors;
        let mut gone = vec![false; colors.len()];
        for c in color.index()..colors.len() {
            let parents = colors.parents(c as u32);
            gone[c] = c == color.index() || parents.iter().any(|p| gone[p.index()]);
            if gone[c] {
                colors.live[c / 64] &= !(1 << (c % 64));
                colors.entries[c] = 0;
                colors.held.iter_mut().for_each(|held| held[c] = 0);
            }
        }
        let tables = once(&mut colors.classes).chain(&mut colors.rows);
        for table in tables {
            let rows: Vec<u32> = (0..table.len() as u32)
                .filter(|&row| table.is_live(row) && gone[table.cell(row, 0) as usize])
                .collect();
            for row in rows {
                Colors::remove(table, row);
            }
            table.maybe_compact();
        }
        colors.unions.retain(|&(c, ..)| !gone[c as usize]);
    }

    /// Unite `a` and `b` in `color`. Rows become canonical in the color at the
    /// next [`Database::rebuild`], as after [`Database::union`].
    pub fn union_in(&mut self, color: Color, a: Value, b: Value) {
        if color == Color::BASE {
            return self.union(a, b);
        }
        self.colors.unions.push((color.0, a.raw(), b.raw()));
    }

    /// [`Database::union_in`] for each color of `colors` and the values at
    /// its position in `a` and `b`.
    pub fn union_in_all(&mut self, colors: &[Color], a: &[Value], b: &[Value]) {
        for ((&color, &a), &b) in colors.iter().zip(a).zip(b) {
            self.union_in(color, a, b);
        }
    }

    /// [`Database::insert`] in `color`: the row holds in the color and every
    /// color above it.
    pub fn insert_in(&mut self, color: Color, table: TableId, row: &[Value]) -> Result<(), Error> {
        if color == Color::BASE {
            return self.insert(table, row);
        }
        self.check_row(table, row)?;
        self.colors.ensure(&self.tables);
        let cells = self.tables[table.index()].canonical(row, &self.uf);
        let cells: Vec<u32> = once(color.0).chain(cells).collect();
        self.write_in(table, cells, false)
    }

    /// The e-class of `value` in `color`.
    pub fn find_in(&self, color: Color, value: Value) -> Value {
        Value::new(self.colors.root(color.0, self.uf.find(value.raw())))
    }

    /// [`Database::lookup`] in `color`.
    pub fn lookup_in(&self, color: Color, table: TableId, key: &[Value]) -> Option<Value> {
        if color == Color::BASE {
            return self.lookup(table, key);
        }
        let t = &self.tables[table.index()];
        let mut cells = vec![color.0];
        cells.extend(t.canonical_key(key, &self.uf));
        for &column in &t.schema.ids {
            if let Some(cell) = cells.get_mut(column + 1) {
                *cell = self.colors.root(color.0, *cell);
            }
        }
        let keys = t.schema.keys;
        if let Some(row) = self.colors.color_row(color.0, table, &cells) {
            let output = self.colors.rows(table).columns.get(keys + 1);
            return Some(Value::new(output.map_or(0, |column| column[row as usize])));
        }
        let row = t.find(&cells[1..], hash_row(&cells[1..]))?;
        if self.colors.holds_moved(color.0, t, row) {
            return None;
        }
        Some(Value::new(
            t.columns.get(keys).map_or(0, |c| c[row as usize]),
        ))
    }

    /// The live rows of `table` in `color`, subsumed ones included: the base
    /// rows the color shows, then the color's own.
    pub fn rows_in(&self, color: Color, table: TableId) -> impl Iterator<Item = Row<'_>> {
        let base = &self.tables[table.index()];
        let colored = color != Color::BASE;
        let shown = (0..base.len() as u32)
            .filter(move |&row| {
                base.is_live(row) && (!colored || self.colors.visible(color.0, table, base, row))
            })
            .map(move |row| Row::new(base, row, 0));
        let own = (self.colors.rows.get(table.index()))
            .filter(|_| colored)
            .into_iter()
            .flat_map(move |rows| {
                (0..rows.len() as u32)
                    .filter(move |&row| rows.is_live(row) && rows.cell(row, 0) == color.0)
                    .map(move |row| Row::new(rows, row, 1))
            });
        shown.chain(own)
    }

    /// The pairs of base e-classes that are one e-class in every color of
    /// `colors`, each pair as (first id of the class, another).
    pub fn common(&self, colors: &[Color]) -> Vec<(Value, Value)> {
        let Some(&first) = colors.first() else {
            return Vec::new();
        };
        // A pair equal in every color is equal in the first, where one of
        // the two moved.
        let classes = &self.colors.classes;
        let mut ids: Vec<u32> = (0..classes.len() as u32)
            .filter(|&row| classes.is_live(row) && classes.cell(row, 0) == first.0)
            .flat_map(|row| [classes.cell(row, 1), classes.cell(row, 2)])
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let mut keyed: Vec<(Vec<u32>, u32)> = (ids.into_iter())
            .map(|id| {
                let key = colors.iter().map(|&c| self.colors.root(c.0, id)).collect();
                (key, id)
            })
            .collect();
        keyed.sort_unstable();
        keyed
            .chunk_by(|a, b| a.0 == b.0)
            .flat_map(|group| {
                let first = Value::new(group[0].1);
                group[1..]
                    .iter()
                    .map(move |&(_, id)| (first, Value::new(id)))
            })
            .collect()
    }

    // Writes `row` of `table`, the color first, in that color: its ids made
    // canonical there.
    pub(crate) fn write_in(
        &mut self,
        table: TableId,
        mut row: Vec<u32>,
        subsumed: bool,
    ) -> Result<(), Error> {
        let c = row[0];
        for &column in &self.colors.rows[table.index()].schema.ids {
            row[column] = self.colors.root(c, self.uf.find(row[column]));
        }
        for order in self.colors.rows[table.index()].orders(&row) {
            self.write_one_in(table, order, subsumed)?;
        }
        Ok(())
    }

    // `row` must be canonical in its color. A row of the color with the key
    // merges as in the base; else a base row the color shows with the key
    // merges into a new row of the color, which hides it from then on, unless
    // the merge leaves it as it is.
    fn write_one_in(
        &mut self,
        table: TableId,
        mut row: Vec<u32>,
        mut subsumed: bool,
    ) -> Result<(), Error> {
        let (t, c, now) = (table.index(), row[0], self.colors.now);
        let rows = &self.colors.rows[t];
        let keys = rows.schema.keys;
        let functional = keys < row.len();
        let hash = hash_row(&row[..keys]);
        if let Some(at) = self.colors.color_row(c, table, &row[..keys]) {
            subsumed |= rows.is_subsumed(at);
            if functional {
                let mut old = rows.cell(at, keys);
                if rows.schema.ids.contains(&keys) {
                    old = self.colors.root(c, self.uf.find(old));
                }
                row[keys] = self.merge_in(c, table, old, row[keys])?;
            }
            let rows = &mut self.colors.rows[t];
            if functional && row[keys] != rows.cell(at, keys) {
                rows.kill(at);
                rows.push(&row, hash, now, subsumed);
            } else if subsumed {
                rows.subsume(at);
            }
            return Ok(());
        }
        let base = &self.tables[t];
        let key = &row[1..keys];
        if let Some(at) = base.find(key, hash_row(key))
            && !self.colors.holds_moved(c, base, at)
        {
            let base_subsumed = base.is_subsumed(at);
            let mut same = true;
            if functional {
                let old = base.cell(at, keys - 1);
                row[keys] = self.merge_in(c, table, old, row[keys])?;
                same = row[keys] == old;
            }
            if same && (base_subsumed || !subsumed) {
                return Ok(());
            }
            subsumed |= base_subsumed;
        }
        self.colors.rows[t].push(&row, hash, now, subsumed);
        self.colors.held[t][c as usize] += 1;
        Ok(())
    }

    fn merge_in(&mut self, c: u32, table: TableId, old: u32, new: u32) -> Result<u32, Error> {
        let (mut ctx, _, colors) = self.parts();
        let tables = ctx.tables;
        let schema = &tables[table.index()].schema;
        let merge = schema.merge.as_ref().expect("a function has a merge");
        let eager = matches!(merge, Merge::Prim(..) | Merge::Const(_));
        if old == new && !eager {
            return Ok(old);
        }
        let (old, new) = (Value::new(old), Value::new(new));
        let merged = eval_in(merge, &mut ctx, &mut colors.unions, c, old, new)?;
        merged.map(Value::raw).ok_or_else(|| Error::Conflict {
            table: schema.name.clone(),
            old,
            new,
        })
    }

    // Applies the unions waiting in the colors, then starts a new color
    // timestamp. Each round rewrites the class maps and relabels the rows
    // that hold a root that lost, which may unite more.
    pub(crate) fn rebuild_colors(&mut self) -> Result<(), Error> {
        while !self.colors.unions.is_empty() {
            let mut unions = std::mem::take(&mut self.colors.unions);
            unions.sort_unstable_by_key(|&(c, ..)| c);
            for unions in unions.chunk_by(|a, b| a.0 == b.0) {
                if self.colors.is_live(unions[0].0) {
                    self.unite(unions)?;
                }
            }
        }
        self.colors.classes.maybe_compact();
        self.colors.rows.iter_mut().for_each(Table::maybe_compact);
        self.colors.now += 1;
        Ok(())
    }

    // One round of the unions of one color.
    fn unite(&mut self, unions: &[(u32, u32, u32)]) -> Result<(), Error> {
        let c = unions[0].0;
        let mut lost: HashMap<u32, u32, FxBuildHasher> = HashMap::default();
        let winner = |lost: &HashMap<u32, u32, FxBuildHasher>, mut root: u32| {
            while let Some(&next) = lost.get(&root) {
                root = next;
            }
            root
        };
        for &(_, a, b) in unions {
            let a = winner(&lost, self.colors.root(c, self.uf.find(a)));
            let b = winner(&lost, self.colors.root(c, self.uf.find(b)));
            if a != b {
                lost.insert(a.max(b), a.min(b));
            }
        }
        if lost.is_empty() {
            return Ok(());
        }
        let mut losers: Vec<u32> = lost.keys().copied().collect();
        losers.sort_unstable();
        let colors = vec![c; losers.len()];
        // The rows that hold a loser, before the class map changes: base rows
        // the color shows, and the color's rows, which go.
        let mut copies = Vec::new();
        for t in 0..self.tables.len() {
            let ids = self.tables[t].schema.ids.clone();
            let mut found = Vec::new();
            for &column in &ids {
                let at = self.tables[t].add_index(&[column]);
                self.tables[t].refresh_index(at);
                holding(&self.tables[t], at, &[&losers[..]], &mut found);
            }
            found.sort_unstable();
            found.dedup();
            let (table, id) = (&self.tables[t], TableId::new(t));
            for row in found {
                if self.colors.visible(c, id, table, row) {
                    let row_cells = once(c).chain(cells(table, row)).collect();
                    copies.push((id, row_cells, table.is_subsumed(row)));
                }
            }
            if self.colors.held[t][c as usize] == 0 {
                continue;
            }
            let rows = &mut self.colors.rows[t];
            let mut found = Vec::new();
            for &column in &ids {
                let at = rows.add_index(&[0, column + 1]);
                rows.refresh_index(at);
                holding(rows, at, &[&colors[..], &losers[..]], &mut found);
            }
            found.sort_unstable();
            found.dedup();
            for &row in &found {
                copies.push((id, cells(rows, row), rows.is_subsumed(row)));
                Colors::remove(rows, row);
            }
            self.colors.held[t][c as usize] -= found.len() as u32;
        }
        let (classes, now) = (&mut self.colors.classes, self.colors.now);
        let at = classes.add_index(&[0, 2]);
        classes.refresh_index(at);
        let mut members = Vec::new();
        holding(classes, at, &[&colors[..], &losers[..]], &mut members);
        let members: Vec<(u32, u32, u32)> = (members.into_iter())
            .map(|row| (row, classes.cell(row, 1), classes.cell(row, 2)))
            .collect();
        for (row, member, root) in members {
            let entry = [c, member, winner(&lost, root)];
            classes.kill(row);
            classes.push(&entry, hash_row(&entry[..2]), now, false);
        }
        for &loser in &losers {
            let entry = [c, loser, winner(&lost, loser)];
            classes.push(&entry, hash_row(&entry[..2]), now, false);
        }
        self.colors.entries[c as usize] += losers.len() as u32;
        for (table, row, subsumed) in copies {
            self.write_in(table, row, subsumed)?;
        }
        Ok(())
    }

    // After a base rebuild: the class maps and color rows move to the new
    // base roots, each color copies the new base rows it does not show, and
    // each level imports what its parents wrote.
    pub(crate) fn rebase_colors(&mut self) -> Result<(), Error> {
        self.colors.ensure(&self.tables);
        let mut hits = Vec::new();
        let moved = take_moved(&mut self.colors.classes, self.uf.parents(), &mut hits);
        for (i, &c) in moved.columns[0].iter().enumerate() {
            self.colors.entries[c as usize] -= 1;
            let (member, root) = (moved.columns[1][i], moved.columns[2][i]);
            self.colors.unions.push((c, member, root));
        }
        let mut copies = Vec::new();
        for t in 0..self.tables.len() {
            let moved = take_moved(&mut self.colors.rows[t], self.uf.parents(), &mut hits);
            for (i, &subsumed) in moved.subsumed.iter().enumerate() {
                let row: Vec<u32> = moved.columns.iter().map(|column| column[i]).collect();
                self.colors.held[t][row[0] as usize] -= 1;
                copies.push((TableId::new(t), row, subsumed));
            }
        }
        for c in 1..self.colors.len() as u32 {
            let since = self.colors.last_base[c as usize];
            if !self.colors.is_live(c) || since == self.now {
                continue;
            }
            for (t, table) in self.tables.iter().enumerate() {
                let id = TableId::new(t);
                for row in table.since(since) {
                    if table.is_live(row) && !self.colors.visible(c, id, table, row) {
                        let row_cells = once(c).chain(cells(table, row)).collect();
                        copies.push((id, row_cells, table.is_subsumed(row)));
                    }
                }
            }
            if self
                .tables
                .iter()
                .any(|table| !table.since(since).is_empty())
            {
                self.colors.rebased[c as usize] = self.colors.now;
            }
            self.colors.last_base[c as usize] = self.now;
        }
        for (table, row, subsumed) in copies {
            self.write_in(table, row, subsumed)?;
        }
        self.rebuild_colors()?;
        for level in self.colors.levels() {
            self.import_colors(&level)?;
        }
        Ok(())
    }

    // Each color of `level` takes the class map entries and rows its parents
    // wrote since it last imported, then the level rebuilds.
    pub(crate) fn import_colors(&mut self, level: &[Color]) -> Result<(), Error> {
        let mut copies = Vec::new();
        for &color in level {
            let (c, since) = (color.0, self.colors.imported[color.index()]);
            for parent in self.colors.parents(c).to_vec() {
                let entries = Colors::rows_of(&mut self.colors.classes, parent.0, since);
                for (entry, _) in entries {
                    self.colors.unions.push((c, entry[1], entry[2]));
                }
                for t in 0..self.colors.rows.len() {
                    let rows = Colors::rows_of(&mut self.colors.rows[t], parent.0, since);
                    for (mut row, subsumed) in rows {
                        row[0] = c;
                        copies.push((TableId::new(t), row, subsumed));
                    }
                }
            }
        }
        for (table, row, subsumed) in copies {
            self.write_in(table, row, subsumed)?;
        }
        self.rebuild_colors()?;
        for &color in level {
            self.colors.imported[color.index()] = self.colors.now;
        }
        Ok(())
    }
}

// The rows of index `at` of `table` whose index columns hold one of the keys,
// given by column, into `out`.
fn holding(table: &Table, at: usize, keys: &[&[u32]], out: &mut Vec<u32>) {
    let index = &table.indexes[at];
    let mut ranges = vec![0..0; keys[0].len()];
    index.ranges(&table.columns, keys, &mut ranges);
    for range in ranges {
        out.extend_from_slice(&index.rows()[range.start as usize..range.end as usize]);
    }
}
