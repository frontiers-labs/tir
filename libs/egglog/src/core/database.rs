use std::cell::Cell;
use std::collections::HashMap;
use std::ops::{Index, Range};
use std::sync::Arc;

use tir_adt::FxBuildHasher;
use tir_adt::simd::hash_row;

use super::base::BaseValues;
use super::color::Colors;
use super::containers::ContainerValues;
use super::group::Group;
use super::schema::{SortSchema, TableSchema};
use super::table::Table;
use super::uf::UnionFind;
use super::value::as_value;
use super::{
    BaseValue, ContainerValue, Error, ExternId, IndexId, Merge, Schema, Shape, Signature, SortId,
    SortKind, TableId, TableKind, Value,
};
use crate::ArcSort;
use crate::Context;
use crate::prelude::BaseSort;
use crate::sort::{BoolSort, EqSort, F64Sort, I64Sort, OpaqueSort, StringSort, UnitSort};

/// The body of a primitive: `None` means the primitive has no result for
/// these arguments.
pub type PrimFn = dyn Fn(&mut PrimCtx<'_>, &[Value]) -> Option<Value> + Send + Sync;

#[derive(Clone)]
pub(crate) struct Prim {
    pub(crate) name: String,
    pub(crate) signature: Signature,
    // Where rules may call it.
    pub(crate) contexts: &'static [Context],
    pub(crate) f: Arc<PrimFn>,
}

#[derive(Clone)]
pub(crate) enum Write {
    Insert(TableId, Vec<Value>),
    Remove(TableId, Vec<Value>),
    Subsume(TableId, Vec<Value>),
}

type Names<T> = HashMap<String, T, FxBuildHasher>;

/// Sorts, tables of rows over them, one union-find over every Eq sort, and
/// primitives on base values.
///
/// A write applies at once. A union makes rows stale until [`Database::rebuild`]
/// canonicalizes them.
#[derive(Clone)]
pub struct Database {
    pub(crate) sorts: Vec<SortSchema>,
    pub(crate) sort_names: Names<SortId>,
    pub(crate) tables: Vec<Table>,
    pub(crate) table_names: Names<TableId>,
    pub(crate) prims: Vec<Prim>,
    pub(crate) indexes: Vec<(TableId, usize)>,
    pub(crate) uf: UnionFind,
    pub(crate) base: BaseValues,
    pub(crate) containers: ContainerValues,
    // Writes of primitives and merges, applied by the next rebuild, and the
    // outputs predicted for the keys they make.
    pub(crate) pending: Vec<Write>,
    pub(crate) predicted: HashMap<(TableId, Vec<u32>), Value, FxBuildHasher>,
    pub(crate) now: u32,
    // Empty until the first color: no color code runs.
    pub(crate) colors: Colors,
    pub(crate) groups: Vec<Group>,
    // By table: the group whose member or keyed table it is.
    pub(crate) group_of_table: Vec<Option<usize>>,
    // (group, id) of each group whose base rows a write changed since the
    // last rebuild, which unites the keyed ones that became equal.
    pub(crate) written_groups: Vec<(usize, u32)>,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    /// A database with the base sorts `i64`, `f64` (as [`F64`](crate::F64)), `bool`,
    /// `String` and `Unit` (as `()`).
    pub fn new() -> Self {
        let mut db = Self {
            sorts: Vec::new(),
            sort_names: HashMap::default(),
            tables: Vec::new(),
            table_names: HashMap::default(),
            prims: Vec::new(),
            indexes: Vec::new(),
            uf: UnionFind::default(),
            base: BaseValues::default(),
            containers: ContainerValues::default(),
            pending: Vec::new(),
            predicted: HashMap::default(),
            now: 0,
            colors: Colors::new(),
            groups: Vec::new(),
            group_of_table: Vec::new(),
            written_groups: Vec::new(),
        };
        db.add_arcsort(I64Sort.to_arcsort());
        db.add_arcsort(F64Sort.to_arcsort());
        db.add_arcsort(BoolSort.to_arcsort());
        db.add_arcsort(StringSort.to_arcsort());
        db.add_arcsort(UnitSort.to_arcsort());
        db
    }

    /// # Panics
    ///
    /// If a sort has this name already.
    pub fn add_sort(&mut self, name: &str, kind: SortKind) -> SortId {
        let name = name.to_owned();
        self.add_arcsort(match kind {
            SortKind::Eq => Arc::new(EqSort { name }),
            kind => Arc::new(OpaqueSort { name, kind }),
        })
    }

    // The sort `sort` describes, with its name.
    pub(crate) fn add_arcsort(&mut self, sort: ArcSort) -> SortId {
        let name = sort.name().to_owned();
        let id = SortId::new(self.sorts.len());
        assert!(
            self.sort_names.insert(name.clone(), id).is_none(),
            "sort `{name}` exists"
        );
        let kind = crate::sort::kind(&*sort);
        self.sorts.push(SortSchema { name, kind, sort });
        id
    }

    /// A table whose columns are all key.
    pub fn add_relation(&mut self, name: &str, columns: &[SortId]) -> TableId {
        self.add_table(name, TableKind::Relation, columns, None, None)
    }

    pub fn add_function(
        &mut self,
        name: &str,
        keys: &[SortId],
        out: SortId,
        merge: Merge,
    ) -> TableId {
        self.add_table(name, TableKind::Function, keys, Some(out), Some(merge))
    }

    /// # Panics
    ///
    /// If `out` is not an Eq sort.
    pub fn add_constructor(&mut self, name: &str, keys: &[SortId], out: SortId) -> TableId {
        assert_eq!(
            self.sorts[out.index()].kind,
            SortKind::Eq,
            "constructor `{name}` makes e-classes"
        );
        self.add_table(
            name,
            TableKind::Constructor,
            keys,
            Some(out),
            Some(Merge::Union),
        )
    }

    fn add_table(
        &mut self,
        name: &str,
        kind: TableKind,
        keys: &[SortId],
        out: Option<SortId>,
        merge: Option<Merge>,
    ) -> TableId {
        let id = TableId::new(self.tables.len());
        assert!(
            self.table_names.insert(name.to_owned(), id).is_none(),
            "table `{name}` exists"
        );
        let columns: Vec<SortId> = keys.iter().copied().chain(out).collect();
        let kind_of = |sort: &SortId| self.sorts[sort.index()].kind;
        let shapes = columns.iter().map(|sort| match kind_of(sort) {
            SortKind::Eq => Shape::Dense,
            _ => Shape::Sparse,
        });
        let ids = (0..columns.len())
            .filter(|&c| kind_of(&columns[c]).has_ids())
            .collect();
        self.tables.push(Table::new(TableSchema {
            name: name.to_owned(),
            kind,
            shapes: shapes.collect(),
            columns,
            keys: keys.len(),
            merge,
            ids,
            symmetries: Vec::new(),
            cost: None,
            unextractable: false,
            default: None,
        }));
        id
    }

    /// The cost of one row of `table` in extraction.
    pub fn set_cost(&mut self, table: TableId, cost: u64) {
        self.tables[table.index()].schema.cost = Some(cost);
    }

    /// Whether extraction skips the rows of `table`.
    pub fn set_unextractable(&mut self, table: TableId, unextractable: bool) {
        self.tables[table.index()].schema.unextractable = unextractable;
    }

    /// The output a function writes for a key that is asked for and missing,
    /// in place of failing.
    ///
    /// # Panics
    ///
    /// If `table` is not a function.
    pub fn set_default(&mut self, table: TableId, default: Value) {
        let schema = &mut self.tables[table.index()].schema;
        assert_eq!(
            schema.kind,
            TableKind::Function,
            "only functions have defaults"
        );
        schema.default = Some(default);
    }

    /// Set the shape of each column, key columns first.
    pub fn set_shapes(&mut self, table: TableId, shapes: &[Shape]) -> Result<(), Error> {
        let table = &mut self.tables[table.index()];
        let expected = table.schema.columns.len();
        if shapes.len() != expected {
            let (table, found) = (table.schema.name.clone(), shapes.len());
            return Err(Error::Shape {
                table,
                expected,
                found,
            });
        }
        table.set_shapes(shapes);
        Ok(())
    }

    /// Add one overload of the primitive `name`, which rules may call
    /// anywhere. An overload with the same signature is replaced.
    pub fn add_primitive(
        &mut self,
        name: &str,
        signature: Signature,
        f: impl Fn(&mut PrimCtx<'_>, &[Value]) -> Option<Value> + Send + Sync + 'static,
    ) -> ExternId {
        self.add_primitive_in(name, signature, &Context::ALL, f)
    }

    /// Add an overload of the primitive `name` that rules may call only
    /// where they run in one of `contexts`: the queries of seminaive rules
    /// are [`Context::Pure`], their actions [`Context::Write`], the queries
    /// of naive rules and checks [`Context::Read`], and their actions and
    /// top-level actions [`Context::Full`].
    pub fn add_primitive_in(
        &mut self,
        name: &str,
        signature: Signature,
        contexts: &'static [Context],
        f: impl Fn(&mut PrimCtx<'_>, &[Value]) -> Option<Value> + Send + Sync + 'static,
    ) -> ExternId {
        let (name, f) = (name.to_owned(), Arc::new(f));
        let prim = Prim {
            name,
            signature,
            contexts,
            f,
        };
        let same = |other: &Prim| other.name == prim.name && other.signature == prim.signature;
        if let Some(at) = self.prims.iter().position(same) {
            self.prims[at] = prim;
            return ExternId::new(at);
        }
        self.prims.push(prim);
        ExternId::new(self.prims.len() - 1)
    }

    pub fn sort(&self, name: &str) -> Result<SortId, Error> {
        self.schema()
            .sort_named(name)
            .ok_or_else(|| Error::UnknownName(name.to_owned()))
    }

    pub fn table(&self, name: &str) -> Result<TableId, Error> {
        self.schema()
            .table_named(name)
            .ok_or_else(|| Error::UnknownName(name.to_owned()))
    }

    pub fn schema(&self) -> Schema<'_> {
        Schema { db: self }
    }

    /// A new e-class of an Eq sort.
    pub fn fresh(&mut self, sort: SortId) -> Value {
        assert_eq!(
            self.sorts[sort.index()].kind,
            SortKind::Eq,
            "fresh ids are e-classes"
        );
        Value::new(self.uf.fresh())
    }

    pub fn base<T: BaseValue>(&self, value: T) -> Value {
        self.base.get(value)
    }

    /// The `T` that `value` stands for; `value` must come from [`Database::base`].
    pub fn base_value<T: BaseValue>(&self, value: Value) -> T {
        self.base.unwrap(value)
    }

    pub fn base_values(&self) -> &BaseValues {
        &self.base
    }

    pub fn container_values(&self) -> &ContainerValues {
        &self.containers
    }

    pub fn container<C: ContainerValue>(&mut self, container: C) -> Value {
        self.containers.intern(container, &mut self.uf)
    }

    pub fn container_value<C: ContainerValue>(&self, value: Value) -> Option<&C> {
        self.containers.get_val(value)
    }

    /// Write a relation row, or a key and its output. A key that has an
    /// output already gets the merge of the two.
    pub fn insert(&mut self, table: TableId, row: &[Value]) -> Result<(), Error> {
        self.check_row(table, row)?;
        let schema = &self.tables[table.index()].schema;
        let cells = self.tables[table.index()].canonical(row, &self.uf);
        let hash = hash_row(&cells[..schema.keys]);
        self.apply_row(table, &cells, hash, false, None)
    }

    // Whether `row` has the columns of `table` and ids where it holds ids.
    pub(crate) fn check_row(&self, table: TableId, row: &[Value]) -> Result<(), Error> {
        let schema = &self.tables[table.index()].schema;
        let (expected, found) = (schema.columns.len(), row.len());
        if expected != found {
            return Err(Error::Arity {
                table: schema.name.clone(),
                expected,
                found,
            });
        }
        if let Some(&column) = schema
            .ids
            .iter()
            .find(|&&c| !self.uf.contains(row[c].raw()))
        {
            let (table, value) = (schema.name.clone(), row[column]);
            return Err(Error::Sort {
                table,
                column,
                value,
            });
        }
        Ok(())
    }

    pub fn remove(&mut self, table: TableId, key: &[Value]) {
        let t = &self.tables[table.index()];
        for key in t.orders(&t.canonical_key(key, &self.uf)) {
            let hash = hash_row(&key);
            if let Some(row) = self.tables[table.index()].find(&key, hash) {
                self.remove_row(table, row, hash);
            }
        }
        self.tables[table.index()].maybe_compact();
    }

    // Captured row IDs stay valid until all deferred removals finish.
    pub(crate) fn remove_row(&mut self, table: TableId, row: u32, hash: u32) {
        let t = &mut self.tables[table.index()];
        if t.is_live(row) {
            if let Some(Some(group)) = self.group_of_table.get(table.index()) {
                self.written_groups.push((*group, t.cell(row, 0)));
            }
            t.remove(row, hash);
        }
    }

    /// Mark a row so rules no longer match it; lookups still see it. A missing
    /// row of a relation or a constructor is added subsumed.
    pub fn subsume(&mut self, table: TableId, key: &[Value]) {
        let t = &self.tables[table.index()];
        let mut row = t.canonical_key(key, &self.uf);
        let hash = hash_row(&row);
        match (t.find(&row, hash), t.schema.kind) {
            (Some(_), _) => {
                let t = &mut self.tables[table.index()];
                for key in t.orders(&row) {
                    if let Some(at) = t.find(&key, hash_row(&key)) {
                        t.subsume(at);
                    }
                }
                return;
            }
            (None, TableKind::Function) => return,
            (None, TableKind::Constructor) => row.push(self.uf.fresh()),
            (None, TableKind::Relation) => {}
        }
        self.apply_row(table, &row, hash, true, None)
            .expect("a row keeps the output it holds");
    }

    /// The output for `key`, or `()` for a relation row that exists.
    pub fn lookup(&self, table: TableId, key: &[Value]) -> Option<Value> {
        self.tables[table.index()].lookup(key, &self.uf)
    }

    /// The live rows of `table`, subsumed ones included.
    pub fn rows(&self, table: TableId) -> Rows<'_> {
        self.rows_since(table, 0)
    }

    /// The live rows of `table` written at timestamp `ts` or later.
    pub fn rows_since(&self, table: TableId, ts: u32) -> Rows<'_> {
        let table = &self.tables[table.index()];
        Rows {
            table,
            ids: RowIds::Log(table.since(ts)),
        }
    }

    // Whether writes or unions wait for a rebuild.
    pub(crate) fn needs_rebuild(&self) -> bool {
        !self.pending.is_empty() || self.uf.unions() > 0
    }

    /// How many tables the database declares.
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// A count that grows with every write to the first `tables` tables, so
    /// equal counts mean none of them changed in between.
    pub fn writes_to_first(&self, tables: usize) -> u64 {
        self.tables[..tables].iter().map(Table::version).sum()
    }

    /// The timestamp writes get now. Each rebuild starts a new one.
    pub fn timestamp(&self) -> u32 {
        self.now
    }

    pub fn union(&mut self, a: Value, b: Value) {
        self.uf.union(a.raw(), b.raw());
    }

    pub fn find(&self, value: Value) -> Value {
        Value::new(self.uf.find(value.raw()))
    }

    /// Every row of `table` a write removes or replaces leaves its cell of
    /// `column` as a row of the one-column relation `log`, which the next
    /// rebuild writes: rules then match what a table no longer holds.
    pub fn log_retired(&mut self, table: TableId, column: usize, log: TableId) {
        self.tables[table.index()].retire = Some((column, log));
    }

    // Writes the cells retired rows left into their logs.
    pub(crate) fn write_retired(&mut self) -> Result<(), Error> {
        for at in 0..self.tables.len() {
            let Some((column, log)) = self.tables[at].retire else {
                continue;
            };
            let id = self.tables[at].schema.ids.contains(&column);
            for cell in std::mem::take(&mut self.tables[at].retired) {
                let cell = if id { self.uf.find(cell) } else { cell };
                self.apply_row(log, &[cell], hash_row(&[cell]), false, None)?;
            }
        }
        Ok(())
    }

    /// Drop every row of `table`.
    pub fn clear(&mut self, table: TableId) {
        self.tables[table.index()].clear();
    }

    /// An index of `table` by the key columns `order`. Declaring the same
    /// order again returns the same index.
    pub fn add_index(&mut self, table: TableId, order: &[usize]) -> IndexId {
        self.add_ordered_index(table, order, &[])
    }

    /// [`Database::add_ordered_index`] that every write to `table` keeps up to
    /// date, so [`Database::listed`] reads it without write access.
    pub fn add_kept_index(&mut self, table: TableId, key: &[usize], by: &[usize]) -> IndexId {
        let order: Vec<usize> = key.iter().chain(by).copied().collect();
        let at = self.tables[table.index()].add_kept_index(&order, key.len());
        self.index_id(table, at)
    }

    /// An index of `table` by the key columns `key` whose rows of a key come
    /// in the order of the columns `by`, as the positions of a list.
    pub fn add_ordered_index(&mut self, table: TableId, key: &[usize], by: &[usize]) -> IndexId {
        let order: Vec<usize> = key.iter().chain(by).copied().collect();
        let at = self.tables[table.index()].add_ordered_index(&order, key.len());
        self.index_id(table, at)
    }

    fn index_id(&mut self, table: TableId, at: usize) -> IndexId {
        let found = self.indexes.iter().position(|&entry| entry == (table, at));
        IndexId::new(found.unwrap_or_else(|| {
            self.indexes.push((table, at));
            self.indexes.len() - 1
        }))
    }

    // A context over the whole database, for primitives, merges and
    // callbacks.
    pub(crate) fn ctx(&mut self) -> PrimCtx<'_> {
        self.ctx_and_indexes().0
    }

    pub(crate) fn with_ctx<R>(&mut self, f: impl FnOnce(&mut PrimCtx<'_>) -> R) -> R {
        f(&mut self.ctx())
    }

    pub(crate) fn ctx_and_indexes(&mut self) -> (PrimCtx<'_>, &[(TableId, usize)]) {
        let (ctx, indexes, _) = self.parts();
        (ctx, indexes)
    }

    pub(crate) fn parts(&mut self) -> (PrimCtx<'_>, &[(TableId, usize)], &mut Colors) {
        let ctx = PrimCtx {
            tables: &self.tables,
            names: &self.table_names,
            sorts: &self.sorts,
            prims: &self.prims,
            uf: &mut self.uf,
            base: &self.base,
            containers: &mut self.containers,
            pending: &mut self.pending,
            predicted: &mut self.predicted,
            groups: &self.groups,
            stop: Cell::new(false),
            panicked: Cell::new(false),
            equality: None,
            context: Context::Full,
        };
        (ctx, &self.indexes, &mut self.colors)
    }

    pub(crate) fn view(&self) -> View<'_> {
        View {
            tables: &self.tables,
            names: &self.table_names,
            sorts: &self.sorts,
            uf: &self.uf,
            base: &self.base,
            containers: &self.containers,
        }
    }
}

// What a reader sees of a database.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    pub(crate) tables: &'a [Table],
    pub(crate) names: &'a Names<TableId>,
    pub(crate) sorts: &'a [SortSchema],
    pub(crate) uf: &'a UnionFind,
    pub(crate) base: &'a BaseValues,
    pub(crate) containers: &'a ContainerValues,
}

impl<'a> View<'a> {
    pub(crate) fn table(&self, name: &str) -> Option<(TableId, &'a TableSchema)> {
        let id = *self.names.get(name)?;
        Some((id, &self.tables[id.index()].schema))
    }

    pub(crate) fn lookup(&self, table: TableId, key: &[Value]) -> Option<Value> {
        self.tables[table.index()].lookup(key, self.uf)
    }

    pub(crate) fn rows(&self, table: TableId) -> Rows<'a> {
        let table = &self.tables[table.index()];
        let ids = RowIds::Log(0..table.len() as u32);
        Rows { table, ids }
    }
}

/// What a primitive can see and do: read and intern base values and
/// containers, look up rows, and write rows. Writes wait for the next
/// [`Database::rebuild`]; a union applies at once.
pub struct PrimCtx<'a> {
    pub(crate) tables: &'a [Table],
    pub(crate) names: &'a Names<TableId>,
    pub(crate) sorts: &'a [SortSchema],
    pub(crate) prims: &'a [Prim],
    pub(crate) uf: &'a mut UnionFind,
    pub(crate) base: &'a BaseValues,
    pub(crate) containers: &'a mut ContainerValues,
    pub(crate) pending: &'a mut Vec<Write>,
    pub(crate) predicted: &'a mut HashMap<(TableId, Vec<u32>), Value, FxBuildHasher>,
    pub(crate) groups: &'a [Group],
    equality: Option<(&'a Colors, u32)>,
    // Set to end the search early; with `panicked`, the search fails.
    pub(crate) stop: Cell<bool>,
    pub(crate) panicked: Cell<bool>,
    pub(crate) context: Context,
}

impl<'a> PrimCtx<'a> {
    pub fn base<T: BaseValue>(&self, value: T) -> Value {
        self.base.get(value)
    }

    pub fn base_value<T: BaseValue>(&self, value: Value) -> T {
        self.base.unwrap(value)
    }

    pub fn base_values(&self) -> &BaseValues {
        self.base
    }

    pub fn container_values(&self) -> &ContainerValues {
        self.containers
    }

    pub fn container<C: ContainerValue>(&mut self, mut container: C) -> Value {
        if self.equality.is_some() {
            container.rebuild_contents(&|value| self.find(value));
        }
        self.containers.intern(container, self.uf)
    }

    pub fn container_value<C: ContainerValue>(&self, value: Value) -> Option<&C> {
        self.containers.get_val(value)
    }

    pub fn find(&self, value: Value) -> Value {
        let root = self.uf.find(value.raw());
        Value::new(
            self.equality
                .map_or(root, |(colors, c)| colors.root(c, root)),
        )
    }

    pub(crate) fn mapped_container<C: ContainerValue>(&self, value: Value) -> Option<C> {
        let mut container = self.container_value::<C>(self.find(value))?.clone();
        if self.equality.is_some() {
            container.rebuild_contents(&|value| self.find(value));
        }
        Some(container)
    }

    /// The table of this name.
    pub fn table(&self, name: &str) -> Option<TableId> {
        self.names.get(name).copied()
    }

    /// The live rows of `table`, subsumed ones included, as they were when
    /// the rules started.
    pub fn rows(&self, table: TableId) -> Rows<'_> {
        self.view().rows(table)
    }

    /// The output for `key`, or the one an earlier write of this context
    /// made for it.
    pub fn lookup(&self, table: TableId, key: &[Value]) -> Option<Value> {
        let t = &self.tables[table.index()];
        t.lookup(key, self.uf).or_else(|| {
            let key = t.canonical(key, self.uf);
            self.predicted.get(&(table, key)).copied()
        })
    }

    /// The output for `key`. A constructor gives a missing key a fresh id, a
    /// function with a default its default, and a relation adds the row.
    pub fn entry(&mut self, table: TableId, key: &[Value]) -> Option<Value> {
        let found = self.lookup(table, key);
        let schema = &self.tables[table.index()].schema;
        let output = match (found, schema.kind) {
            (Some(found), _) => return Some(found),
            (None, TableKind::Constructor) => Some(Value::new(self.uf.fresh())),
            (None, TableKind::Function) => Some(schema.default?),
            (None, TableKind::Relation) => None,
        };
        self.insert(table, &[key, output.as_slice()].concat());
        let value = output.unwrap_or(Value::new(0));
        let key = self.tables[table.index()].canonical(key, self.uf);
        self.predicted.insert((table, key), value);
        Some(value)
    }

    pub fn call(&mut self, prim: ExternId, args: &[Value]) -> Option<Value> {
        let prims = self.prims;
        if !prims[prim.index()].contexts.contains(&self.context) {
            self.panic();
            return None;
        }
        let mut normalized = Vec::new();
        let args = if let Some((colors, c)) = self.equality {
            super::color::check_pure(self, prim).ok()?;
            let signature = &prims[prim.index()].signature;
            normalized.reserve(args.len());
            for (&value, &sort) in args.iter().zip(&signature.inputs) {
                let value = match self.sorts[sort.index()].kind {
                    SortKind::Base(_) => value,
                    SortKind::Eq => self.find(value),
                    SortKind::Container(ty) => {
                        let value = self.find(value);
                        let find =
                            |v: Value, uf: &UnionFind| Value::new(colors.root(c, uf.find(v.raw())));
                        self.containers.map(ty, value, &find, self.uf)?
                    }
                };
                normalized.push(value);
            }
            normalized.as_slice()
        } else {
            args
        };
        let value = (prims[prim.index()].f)(self, args)?;
        if self.panicked.get() {
            return None;
        }
        Some(
            if self.equality.is_some()
                && !matches!(
                    self.sorts[prims[prim.index()].signature.output.index()].kind,
                    SortKind::Base(_)
                )
            {
                self.find(value)
            } else {
                value
            },
        )
    }

    pub(crate) fn call_in(
        &mut self,
        colors: &'a Colors,
        c: u32,
        prim: ExternId,
        args: &[Value],
    ) -> Option<Value> {
        let previous = self.equality.replace((colors, c));
        let result = self.call(prim, args);
        self.equality = previous;
        result
    }

    /// The primitive overloads of this name.
    pub fn primitives(&self, name: &str) -> impl Iterator<Item = (ExternId, &Signature)> {
        let prims = self.prims.iter().enumerate();
        let named = prims.filter(move |(_, prim)| prim.name == name);
        named.map(|(i, prim)| (ExternId::new(i), &prim.signature))
    }

    pub fn insert(&mut self, table: TableId, row: &[Value]) {
        self.pending.push(Write::Insert(table, row.to_vec()));
    }

    pub fn remove(&mut self, table: TableId, key: &[Value]) {
        self.pending.push(Write::Remove(table, key.to_vec()));
    }

    /// Hide a row from rules. A missing row of a constructor or relation is
    /// made, with the output a later [`PrimCtx::entry`] of the key gets.
    pub fn subsume(&mut self, table: TableId, key: &[Value]) {
        if self.tables[table.index()].schema.kind != TableKind::Function {
            self.entry(table, key);
        }
        self.pending.push(Write::Subsume(table, key.to_vec()));
    }

    pub fn union(&mut self, a: Value, b: Value) {
        self.uf.union(a.raw(), b.raw());
    }

    /// Ask the running rules to stop after this batch.
    pub fn stop(&self) {
        self.stop.set(true);
    }

    /// Stop the running rules with an error.
    pub fn panic(&self) {
        self.stop.set(true);
        self.panicked.set(true);
    }

    pub(crate) fn view(&self) -> View<'_> {
        View {
            tables: self.tables,
            names: self.names,
            sorts: self.sorts,
            uf: self.uf,
            base: self.base,
            containers: self.containers,
        }
    }
}

/// Live rows of one table.
pub struct Rows<'a> {
    pub(crate) table: &'a Table,
    pub(crate) ids: RowIds<'a>,
}

pub(crate) enum RowIds<'a> {
    Log(Range<u32>),
    Listed(std::slice::Iter<'a, u32>),
    Owned(std::vec::IntoIter<u32>),
}

impl<'a> Iterator for Rows<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Row<'a>> {
        loop {
            let row = match &mut self.ids {
                RowIds::Log(rows) => rows.next()?,
                RowIds::Listed(rows) => *rows.next()?,
                RowIds::Owned(rows) => rows.next()?,
            };
            if self.table.is_live(row) {
                return Some(Row::new(self.table, row, 0));
            }
        }
    }
}

/// One row: its key columns, then its output column. Index it by column.
#[derive(Clone, Copy)]
pub struct Row<'a> {
    table: &'a Table,
    row: u32,
    // The table column of the row's first column: 1 in a color table.
    first: usize,
}

impl<'a> Row<'a> {
    pub(crate) fn new(table: &'a Table, row: u32, first: usize) -> Self {
        Self { table, row, first }
    }

    pub fn values(self) -> impl Iterator<Item = Value> + 'a {
        let (table, row) = (self.table, self.row);
        (self.first..table.columns.len()).map(move |c| Value::new(table.cell(row, c)))
    }

    pub fn is_subsumed(self) -> bool {
        self.table.is_subsumed(self.row)
    }

    pub fn timestamp(self) -> u32 {
        self.table.timestamp(self.row)
    }
}

impl Index<usize> for Row<'_> {
    type Output = Value;

    fn index(&self, column: usize) -> &Value {
        as_value(&self.table.columns[self.first + column][self.row as usize])
    }
}
