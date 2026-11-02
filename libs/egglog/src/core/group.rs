use std::iter::once;

use tir_adt::simd::hash_row;

use super::color::Colors;
use super::database::Row;
use super::keymap::EMPTY;
use super::rebuild::Delta;
use super::table::Table;
use super::uf::UnionFind;
use super::value::as_cells;
use super::{Color, Database, Error, SortId, SortKind, TableId, TableKind, Value};

// The groups of one sort. A group is the rows whose first column holds its
// id, in each member table.
#[derive(Clone)]
pub(crate) struct Group {
    pub(crate) sort: SortId,
    // The relation that marks the groups find-or-make finds and a rebuild
    // merges.
    pub(crate) keyed: TableId,
    pub(crate) members: Vec<Member>,
}

#[derive(Clone)]
pub(crate) struct Member {
    pub(crate) table: TableId,
    // Kept indexes of the table: by every column but the group, and by the
    // group; then their mirrors in its color table, the color first.
    by_cells: (usize, usize),
    by_group: (usize, usize),
}

// A row of a group: its table, and its cells after the group column.
pub(crate) type GroupRow = (TableId, Vec<u32>);

// What find-or-make reads: the base, or the rows a color shows and its own.
#[derive(Clone, Copy)]
pub(crate) struct GroupView<'a> {
    pub(crate) tables: &'a [Table],
    pub(crate) uf: &'a UnionFind,
    pub(crate) colors: Option<(&'a Colors, u32)>,
}

// The rows of one key of an index and of its color mirror; some are dead or
// hidden from the view.
struct Hits<'a> {
    table: TableId,
    base: (&'a Table, &'a [u32], Vec<u32>),
    color: Option<(&'a Table, &'a [u32], Vec<u32>)>,
}

impl Hits<'_> {
    fn len(&self) -> usize {
        let (_, laid, tail) = &self.base;
        let color = self
            .color
            .as_ref()
            .map_or(0, |(_, laid, tail)| laid.len() + tail.len());
        laid.len() + tail.len() + color
    }
}

impl Group {
    fn member(&self, table: TableId) -> &Member {
        let member = self.members.iter().find(|member| member.table == table);
        member.expect("a member table of the group")
    }
}

// The group of `groups` that `table` is a member table of.
pub(crate) fn group_at(groups: &[Group], table: TableId) -> Option<usize> {
    let has = |group: &Group| group.members.iter().any(|member| member.table == table);
    groups.iter().position(has)
}

impl<'a> GroupView<'a> {
    // Makes the ids of `cells`, a row of `table` after its group column,
    // canonical in the view.
    pub(crate) fn canonical(&self, table: TableId, cells: &mut [u32]) {
        for &column in &self.tables[table.index()].schema.ids {
            if let Some(cell) = column.checked_sub(1).and_then(|c| cells.get_mut(c)) {
                *cell = self.root(*cell);
            }
        }
    }

    fn root(&self, id: u32) -> u32 {
        let id = self.uf.find(id);
        self.colors.map_or(id, |(colors, c)| colors.root(c, id))
    }

    fn hits(&self, table: TableId, (at, mirror): (usize, usize), key: &[u32]) -> Hits<'a> {
        let base = &self.tables[table.index()];
        let (laid, tail) = base.indexes[at].rows_for(&base.columns, key);
        let color = self.colors.map(|(colors, c)| {
            let rows = colors.rows(table);
            let key: Vec<u32> = once(c).chain(key.iter().copied()).collect();
            let (laid, tail) = rows.indexes[mirror].rows_for(&rows.columns, &key);
            (rows, laid, tail)
        });
        Hits {
            table,
            base: (base, laid, tail),
            color,
        }
    }

    // The live rows of `hits` the view shows: the base ones, then the color's.
    fn shown(self, hits: Hits<'a>) -> impl Iterator<Item = Row<'a>> {
        let (table, (base, laid, tail)) = (hits.table, hits.base);
        let visible = move |row: &u32| {
            base.is_live(*row)
                && (self.colors).is_none_or(|(colors, c)| colors.visible(c, table, base, *row))
        };
        let base = (laid.iter().copied().chain(tail))
            .filter(visible)
            .map(move |row| Row::new(base, row, 0));
        let color = hits.color.into_iter().flat_map(|(rows, laid, tail)| {
            (laid.iter().copied().chain(tail))
                .filter(|&row| rows.is_live(row))
                .map(move |row| Row::new(rows, row, 1))
        });
        base.chain(color)
    }

    fn is_keyed(&self, group: &Group, g: u32) -> bool {
        let keyed = &self.tables[group.keyed.index()];
        let Some((colors, c)) = self.colors else {
            return keyed.find(&[g], hash_row(&[g])).is_some();
        };
        colors.color_row(c, group.keyed, &[c, g]).is_some()
            || (keyed.find(&[g], hash_row(&[g])))
                .is_some_and(|row| !colors.hides(c, group.keyed, keyed, row))
    }

    // The rows of the group `g`, sorted.
    pub(crate) fn group_rows(&self, group: &Group, g: u32) -> Vec<GroupRow> {
        let mut rows = Vec::new();
        for member in &group.members {
            let hits = self.hits(member.table, member.by_group, &[g]);
            let cells = |row: Row<'_>| row.values().skip(1).map(Value::raw).collect();
            rows.extend(self.shown(hits).map(|row| (member.table, cells(row))));
        }
        rows.sort_unstable();
        rows
    }

    // The keyed group other than `skip` whose rows are `rows`, sorted and
    // canonical: of the groups that hold a row of `rows`, through the
    // shortest list.
    pub(crate) fn find(&self, group: &Group, rows: &[GroupRow], skip: u32) -> Option<u32> {
        let hits = (rows.iter())
            .map(|(table, cells)| self.hits(*table, group.member(*table).by_cells, cells));
        let fewest = hits.min_by_key(Hits::len)?;
        self.shown(fewest)
            .map(|row| row[0].raw())
            .find(|&g| g != skip && self.is_keyed(group, g) && self.group_rows(group, g) == rows)
    }
}

impl Database {
    /// Declare the groups of `sort`: a group is the rows whose first column
    /// holds its id in each of `tables`. `keyed`, a relation over `sort`,
    /// marks the groups that [`Database::find_or_make`] finds, and a rebuild
    /// unites two keyed groups whose rows become equal.
    pub fn add_group(
        &mut self,
        sort: SortId,
        tables: &[TableId],
        keyed: TableId,
    ) -> Result<(), Error> {
        let schema = |table: &TableId| &self.tables[table.index()].schema;
        let unsupported = |what: &str| Err(Error::Unsupported(what.to_owned()));
        if self.sorts[sort.index()].kind != SortKind::Eq {
            return unsupported("a group sort is an Eq sort");
        }
        if schema(&keyed).kind != TableKind::Relation || schema(&keyed).columns != [sort] {
            return unsupported("`keyed` is a relation over the group sort alone");
        }
        let member = |table| matches!(&schema(table).columns[..], [first, _, ..] if *first == sort);
        if !tables.iter().all(member) {
            return unsupported("a member table starts with the group column and has others");
        }
        let members = (tables.iter())
            .map(|&table| {
                let t = &mut self.tables[table.index()];
                let len = t.schema.columns.len();
                let order: Vec<usize> = (1..len).chain([0]).collect();
                let by_cells = t.add_kept_index(&order, len - 1);
                let by_group = t.add_kept_index(&[0], 1);
                Member {
                    table,
                    by_cells: (by_cells, 0),
                    by_group: (by_group, 0),
                }
            })
            .collect();
        for &table in tables.iter().chain([&keyed]) {
            self.group_of_table.resize(self.tables.len(), None);
            self.group_of_table[table.index()] = Some(self.groups.len());
        }
        self.groups.push(Group {
            sort,
            keyed,
            members,
        });
        Ok(())
    }

    /// The keyed group whose rows are exactly `rows`, each a member table of
    /// one group and the cells of its row after the group column; else a new
    /// group with these rows, marked keyed.
    pub fn find_or_make(&mut self, rows: &[(TableId, &[Value])]) -> Result<Value, Error> {
        self.find_or_make_in(Color::BASE, rows)
    }

    /// The keyed group whose rows are exactly `rows`, as
    /// [`Database::find_or_make`] would find it, without making one.
    pub fn find_group(&self, rows: &[(TableId, &[Value])]) -> Result<Option<Value>, Error> {
        let at = self.group_of(rows)?;
        let view = self.group_view(Color::BASE);
        let mut spec: Vec<GroupRow> = (rows.iter())
            .map(|&(table, cells)| {
                let mut cells = as_cells(cells).to_vec();
                view.canonical(table, &mut cells);
                (table, cells)
            })
            .collect();
        spec.sort_unstable();
        Ok(view.find(&self.groups[at], &spec, EMPTY).map(Value::new))
    }

    /// [`Database::find_or_make`] in `color`: it finds the base rows the
    /// color shows and the color's own, and makes new rows in the color.
    pub fn find_or_make_in(
        &mut self,
        color: Color,
        rows: &[(TableId, &[Value])],
    ) -> Result<Value, Error> {
        let at = self.group_of(rows)?;
        if color != Color::BASE {
            self.colors.ensure(&self.tables);
            self.refresh_group_mirrors();
        }
        let view = self.group_view(color);
        let mut spec: Vec<GroupRow> = (rows.iter())
            .map(|&(table, cells)| {
                let mut cells = as_cells(cells).to_vec();
                view.canonical(table, &mut cells);
                (table, cells)
            })
            .collect();
        spec.sort_unstable();
        let group = &self.groups[at];
        if let Some(g) = view.find(group, &spec, EMPTY) {
            return Ok(Value::new(g));
        }
        let (sort, keyed) = (group.sort, group.keyed);
        let g = self.fresh(sort);
        for &(table, cells) in rows {
            self.insert_in(color, table, &[&[g], cells].concat())?;
        }
        self.insert_in(color, keyed, &[g])?;
        Ok(g)
    }

    // The group whose member tables `rows` write, with each row checked.
    fn group_of(&self, rows: &[(TableId, &[Value])]) -> Result<usize, Error> {
        let at = rows.first().and_then(|&(table, _)| self.group_at(table));
        let at =
            at.ok_or_else(|| Error::Unsupported("find-or-make writes rows of a group".to_owned()))?;
        for &(table, cells) in rows {
            let schema = &self.tables[table.index()].schema;
            if self.group_at(table) != Some(at) {
                let name = &schema.name;
                let message = format!("`{name}` is no member table of the group");
                return Err(Error::Unsupported(message));
            }
            if cells.len() + 1 != schema.columns.len() {
                return Err(Error::Arity {
                    table: schema.name.clone(),
                    expected: schema.columns.len() - 1,
                    found: cells.len(),
                });
            }
        }
        Ok(at)
    }

    pub(crate) fn group_at(&self, table: TableId) -> Option<usize> {
        group_at(&self.groups, table)
    }

    pub(crate) fn group_view(&self, color: Color) -> GroupView<'_> {
        GroupView {
            tables: &self.tables,
            uf: &self.uf,
            colors: (color != Color::BASE).then_some((&self.colors, color.index() as u32)),
        }
    }

    // Builds the mirrors of the group indexes in the color tables, for reads
    // in a color.
    pub(crate) fn refresh_group_mirrors(&mut self) {
        for member in self.groups.iter_mut().flat_map(|group| &mut group.members) {
            let len = self.tables[member.table.index()].schema.columns.len();
            let order: Vec<usize> = once(0).chain(2..=len).chain([1]).collect();
            member.by_cells.1 = self.colors.refresh(member.table, &order);
            member.by_group.1 = self.colors.refresh(member.table, &[0, 1]);
        }
    }

    // The groups whose rows `delta` of `table` wrote, with the group of each.
    pub(crate) fn touched_groups(
        &self,
        table: TableId,
        delta: &Delta,
        out: &mut Vec<(usize, u32)>,
    ) {
        if let Some(at) = self.group_at(table) {
            out.extend(delta.columns[0].iter().map(|&g| (at, g)));
        }
    }

    /// Unites each keyed group whose base rows a write changed since the
    /// last call with a keyed group whose rows equal its own; whether it
    /// united any. A rebuild calls it first.
    pub fn unite_written(&mut self) -> bool {
        if self.written_groups.is_empty() {
            return false;
        }
        let before = self.uf.unions();
        let written: Vec<(usize, u32)> = std::mem::take(&mut self.written_groups)
            .into_iter()
            .map(|(at, g)| (at, self.uf.find(g)))
            .collect();
        self.unite_congruent(written);
        self.uf.unions() != before
    }

    // Unites each keyed group of `touched` with another keyed group whose
    // rows equal its own.
    pub(crate) fn unite_congruent(&mut self, mut touched: Vec<(usize, u32)>) {
        touched.sort_unstable();
        touched.dedup();
        let view = self.group_view(Color::BASE);
        let equal: Vec<(u32, u32)> = (touched.into_iter())
            .filter(|&(at, g)| view.is_keyed(&self.groups[at], g))
            .filter_map(|(at, g)| {
                let group = &self.groups[at];
                let rows = view.group_rows(group, g);
                view.find(group, &rows, g).map(|other| (g, other))
            })
            .collect();
        for (a, b) in equal {
            self.uf.union(a, b);
        }
    }
}
