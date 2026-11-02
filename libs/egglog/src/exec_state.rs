// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/exec_state.rs and src/api/mod.rs. See libs/egglog/LICENSE-MIT.

//! What a primitive or rule callback can reach of a database, by context,
//! and the conversions between Rust values and rows.
//!
//! | Wrapper        | reads | writes | runs in                              |
//! |----------------|-------|--------|--------------------------------------|
//! | [`PureState`]  | no    | no     | queries of rules                     |
//! | [`WriteState`] | no    | yes    | actions of rules                     |
//! | [`ReadState`]  | yes   | no     | [`EGraph::read`](crate::EGraph::read) |
//! | [`FullState`]  | yes   | yes    | [`EGraph::update`](crate::EGraph::update), naive rules |

use std::any::TypeId;
use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;

use crate::core::View;
use crate::sort::{BigInt, BigRat, FunctionContainer};
use crate::{
    ArcSort, BaseValue, BaseValues, ContainerValue, ContainerValues, Error, ExternId, F64,
    FuncType, FunctionSubtype, PrimCtx, TableId, TableKind, TableSchema, Value,
};

/// A misuse of a table that the Rust API catches at run time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// The declarations are out of reach of this state.
    SchemasUnavailable {
        name: String,
    },
    MissingTable {
        name: String,
    },
    WrongSubtype {
        name: String,
        expected: &'static str,
        actual: &'static str,
    },
    WrongArity {
        table: String,
        expected: usize,
        got: usize,
    },
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemasUnavailable { name } => write!(
                f,
                "declarations are not available in this execution, so the signature of `{name}` cannot be read"
            ),
            Self::MissingTable { name } => write!(f, "no table named `{name}` is registered"),
            Self::WrongSubtype {
                name,
                expected,
                actual,
            } => write!(
                f,
                "table `{name}` is a {actual}; this method is only valid for {expected} tables"
            ),
            Self::WrongArity {
                table,
                expected,
                got,
            } => write!(
                f,
                "table `{table}`: expected {expected} input columns, got {got}"
            ),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<ApiError> for Error {
    fn from(error: ApiError) -> Self {
        Self::ApiError(error)
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A Rust value that converts into one cell.
pub trait IntoValue {
    fn into_value(self, bv: &BaseValues) -> Value;
}

/// A Rust value that one cell converts into.
pub trait FromValue: Sized {
    fn from_value(value: Value, bv: &BaseValues) -> Self;
}

/// A row of cells: one [`IntoValue`], a tuple of up to eight, or
/// [`RawValues`].
pub trait IntoValues: sealed::Sealed {
    fn into_values(self, bv: &BaseValues) -> impl Iterator<Item = Value>;
}

/// What a row of cells converts into: `()`, `Vec<Value>`, or a tuple of up
/// to eight [`FromValue`]s.
pub trait FromValues: Sized + sealed::Sealed {
    fn from_values(values: &[Value], bv: &BaseValues) -> Self;
}

macro_rules! impl_value_for_base {
    ($($ty:ty),+) => {$(
        impl IntoValue for $ty {
            fn into_value(self, bv: &BaseValues) -> Value {
                bv.get::<$ty>(self)
            }
        }

        impl FromValue for $ty {
            fn from_value(value: Value, bv: &BaseValues) -> Self {
                bv.unwrap::<$ty>(value)
            }
        }
    )+};
}

impl_value_for_base!(i64, bool, (), F64, String, BigInt, BigRat);

impl IntoValue for &str {
    fn into_value(self, bv: &BaseValues) -> Value {
        bv.get(self.to_owned())
    }
}

impl IntoValue for f64 {
    fn into_value(self, bv: &BaseValues) -> Value {
        bv.get(F64(self))
    }
}

impl FromValue for f64 {
    fn from_value(value: Value, bv: &BaseValues) -> Self {
        bv.unwrap::<F64>(value).0
    }
}

impl IntoValue for Value {
    fn into_value(self, _bv: &BaseValues) -> Value {
        self
    }
}

impl FromValue for Value {
    fn from_value(value: Value, _bv: &BaseValues) -> Self {
        value
    }
}

/// A row of cells as they are, such as the empty key `RawValues(vec![])`.
#[derive(Clone, Debug)]
pub struct RawValues(pub Vec<Value>);

impl sealed::Sealed for RawValues {}

impl IntoValues for RawValues {
    fn into_values(self, _bv: &BaseValues) -> impl Iterator<Item = Value> {
        self.0.into_iter()
    }
}

impl sealed::Sealed for Vec<Value> {}

impl FromValues for Vec<Value> {
    fn from_values(values: &[Value], _bv: &BaseValues) -> Self {
        values.to_vec()
    }
}

impl FromValues for () {
    fn from_values(_values: &[Value], _bv: &BaseValues) -> Self {}
}

impl<A: IntoValue> sealed::Sealed for A {}

impl<A: IntoValue> IntoValues for A {
    fn into_values(self, bv: &BaseValues) -> impl Iterator<Item = Value> {
        std::iter::once(self.into_value(bv))
    }
}

macro_rules! impl_values_for_tuple {
    ($(($($name:ident),+)),+) => {$(
        #[allow(non_snake_case)]
        impl<$($name),+> sealed::Sealed for ($($name,)+) {}

        #[allow(non_snake_case)]
        impl<$($name: IntoValue),+> IntoValues for ($($name,)+) {
            fn into_values(self, bv: &BaseValues) -> impl Iterator<Item = Value> {
                let ($($name,)+) = self;
                [$($name.into_value(bv)),+].into_iter()
            }
        }

        #[allow(non_snake_case)]
        impl<$($name: FromValue),+> FromValues for ($($name,)+) {
            fn from_values(values: &[Value], bv: &BaseValues) -> Self {
                let mut values = values.iter().copied();
                let row = ($($name::from_value(values.next().expect("a row as long as the tuple"), bv),)+);
                assert!(values.next().is_none(), "a row as long as the tuple");
                row
            }
        }
    )+};
}

impl_values_for_tuple! {
    (T1), (T1, T2), (T1, T2, T3), (T1, T2, T3, T4), (T1, T2, T3, T4, T5),
    (T1, T2, T3, T4, T5, T6), (T1, T2, T3, T4, T5, T6, T7), (T1, T2, T3, T4, T5, T6, T7, T8)
}

/// Where a primitive runs, named after the state it gets: the queries of
/// seminaive rules are `Pure` and their actions `Write`; the queries of
/// naive rules and checks are `Read`, and their actions and top-level
/// actions `Full`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Context {
    Pure,
    Write,
    Read,
    Full,
}

impl Context {
    pub const ALL: [Self; 4] = [Self::Pure, Self::Write, Self::Read, Self::Full];
}

// What every state reaches: a view of the database, and for all but a
// `ReadState` of `EGraph::read`, a context that can intern and write.
#[allow(private_interfaces)]
pub(crate) trait Internal<'db> {
    fn view(&self) -> View<'_>;
    fn ctx(&mut self) -> &mut PrimCtx<'db>;
    fn stop_flag(&self) -> Option<&Cell<bool>>;
}

/// Base values, containers and conversions, in every state.
#[allow(private_bounds)]
pub trait Core<'a, 'db: 'a>: Internal<'db> {
    fn base_values(&self) -> &BaseValues {
        self.view().base
    }

    fn container_values(&self) -> &ContainerValues {
        self.view().containers
    }

    /// Ask the running rules to stop after this batch of matches.
    fn trigger_early_stop(&self) {
        self.stop_flag().inspect(|stop| stop.set(true));
    }

    fn should_stop(&self) -> bool {
        self.stop_flag().is_some_and(Cell::get)
    }

    fn register_container<C: ContainerValue>(&mut self, container: C) -> Value {
        self.ctx().container(container)
    }

    /// The container of type `type_id` at `value` with `remap` applied to
    /// its ids, interned; `None` if `value` is no such container.
    fn map_container(
        &mut self,
        type_id: TypeId,
        value: Value,
        remap: &(dyn Fn(Value) -> Value + Send + Sync),
    ) -> Option<Value> {
        let ctx = self.ctx();
        ctx.containers.map(type_id, value, &|v, _| remap(v), ctx.uf)
    }

    fn value_to_base<T: BaseValue>(&self, x: Value) -> T {
        self.base_values().unwrap(x)
    }

    fn base_to_value<T: BaseValue>(&self, x: T) -> Value {
        self.base_values().get(x)
    }

    fn value_to_container<T: ContainerValue>(&self, x: Value) -> Option<&T> {
        self.container_values().get_val(x)
    }

    fn container_to_value<T: ContainerValue>(&mut self, x: T) -> Value {
        self.register_container(x)
    }

    /// Calls an `UnstableFn` value.
    fn apply_function(&mut self, fc: &FunctionContainer, args: &[Value]) -> Option<Value> {
        fc.apply(self.ctx(), args)
    }

    fn apply_primitive(&mut self, primitive: ExternId, args: &[Value]) -> Option<Value> {
        self.ctx().call(primitive, args)
    }
}

// The kind of a table as the Rust API names it; a relation counts as a
// constructor.
fn label(kind: TableKind) -> &'static str {
    match kind {
        TableKind::Function => "function",
        TableKind::Constructor | TableKind::Relation => "constructor",
    }
}

fn table<'v>(
    view: &View<'v>,
    name: &str,
    kind: Option<TableKind>,
) -> Result<(TableId, &'v TableSchema), Error> {
    let missing = || ApiError::MissingTable {
        name: name.to_owned(),
    };
    let (id, schema) = view.table(name).ok_or_else(missing)?;
    match kind {
        Some(kind) if label(kind) != label(schema.kind) => {
            Err(Error::ApiError(ApiError::WrongSubtype {
                name: name.to_owned(),
                expected: label(kind),
                actual: label(schema.kind),
            }))
        }
        _ => Ok((id, schema)),
    }
}

// The table `name` of kind `kind`, if given, and the cells of `key`.
fn keyed(
    view: &View<'_>,
    name: &str,
    kind: Option<TableKind>,
    key: impl IntoValues,
) -> Result<(TableId, Vec<Value>), Error> {
    let (id, schema) = table(view, name, kind)?;
    Ok((id, self::key(schema, key, view.base)?))
}

// The cells of `key`, which must fill the key columns of `schema`.
fn key(schema: &TableSchema, key: impl IntoValues, bv: &BaseValues) -> Result<Vec<Value>, Error> {
    let key: Vec<Value> = key.into_values(bv).collect();
    if key.len() != schema.keys {
        let (table, expected, got) = (schema.name.clone(), schema.keys, key.len());
        return Err(Error::ApiError(ApiError::WrongArity {
            table,
            expected,
            got,
        }));
    }
    Ok(key)
}

fn schema_of(view: &View<'_>, name: &str, kind: TableKind) -> Result<FuncType, Error> {
    let (_, schema) = table(view, name, Some(kind))?;
    Ok(FuncType::of(schema, view.sorts))
}

/// One row of a constructor or relation; a relation row's eclass is `()`.
#[derive(Clone, Copy, Debug)]
pub struct Enode<'a> {
    pub name: &'a str,
    pub children: &'a [Value],
    pub eclass: Value,
    pub subsumed: bool,
}

/// One row of a function.
#[derive(Clone, Copy, Debug)]
pub struct FunctionEntry<'a> {
    pub inputs: &'a [Value],
    pub output: Value,
    pub subsumed: bool,
}

// Calls `f` with the keys, output and subsumption of the rows of `id` until
// it returns false.
fn rows_while(view: &View<'_>, id: TableId, mut f: impl FnMut(&[Value], Value, bool) -> bool) {
    let keys = view.tables[id.index()].schema.keys;
    let unit = view.base.get(());
    let mut row = Vec::new();
    for found in view.rows(id) {
        row.clear();
        row.extend(found.values());
        let output = row.get(keys).copied().unwrap_or(unit);
        if !f(&row[..keys], output, found.is_subsumed()) {
            break;
        }
    }
}

/// Lookups and scans by table name, in [`ReadState`] and [`FullState`].
#[allow(private_bounds)]
pub trait Read<'a, 'db: 'a>: Core<'a, 'db> {
    /// The output of a function for `key`.
    fn lookup<K: IntoValues>(&self, name: &str, key: K) -> Result<Option<Value>, Error> {
        let view = self.view();
        let (id, key) = keyed(&view, name, Some(TableKind::Function), key)?;
        Ok(view.lookup(id, &key))
    }

    /// The e-class of a constructor row, without making one.
    fn eclass_of<K: IntoValues>(&self, name: &str, inputs: K) -> Result<Option<Value>, Error> {
        let view = self.view();
        let (id, key) = keyed(&view, name, Some(TableKind::Constructor), inputs)?;
        Ok(view.lookup(id, &key))
    }

    fn contains<K: IntoValues>(&self, name: &str, key: K) -> Result<bool, Error> {
        let view = self.view();
        let (id, key) = keyed(&view, name, None, key)?;
        Ok(view.lookup(id, &key).is_some())
    }

    fn constructor_schema(&self, name: &str) -> Result<FuncType, Error> {
        schema_of(&self.view(), name, TableKind::Constructor)
    }

    fn function_schema(&self, name: &str) -> Result<FuncType, Error> {
        schema_of(&self.view(), name, TableKind::Function)
    }

    fn table_subtype(&self, name: &str) -> Option<FunctionSubtype> {
        let (_, schema) = self.view().table(name)?;
        Some(FunctionSubtype::of(schema.kind))
    }

    /// The live rows of the table, subsumed ones included.
    fn table_size(&self, name: &str) -> Option<usize> {
        let view = self.view();
        Some(view.rows(view.table(name)?.0).count())
    }

    /// The tables and their sizes, internal tables left out.
    fn table_sizes(&self) -> Vec<(&str, usize)> {
        let view = self.view();
        let tables = view.tables.iter().enumerate();
        let visible = tables.filter(|(_, table)| !table.schema.name.starts_with('@'));
        visible
            .map(|(i, table)| {
                (
                    table.schema.name.as_str(),
                    view.rows(TableId::new(i)).count(),
                )
            })
            .collect()
    }

    fn tables(&self) -> impl Iterator<Item = &str> {
        self.table_sizes().into_iter().map(|(name, _)| name)
    }

    fn is_table_hidden(&self, name: &str) -> Option<bool> {
        self.view()
            .table(name)
            .map(|(_, schema)| schema.name.starts_with('@'))
    }

    fn is_sort_unionable(&self, sort: &ArcSort) -> Option<bool> {
        Some(sort.is_eq_sort())
    }

    fn constructor_enodes(&self, name: &str, mut f: impl FnMut(Enode<'_>)) -> Result<(), Error> {
        self.constructor_enodes_while(name, |enode| {
            f(enode);
            true
        })
    }

    fn constructor_enodes_while(
        &self,
        name: &str,
        mut f: impl FnMut(Enode<'_>) -> bool,
    ) -> Result<(), Error> {
        let view = self.view();
        let (id, _) = table(&view, name, Some(TableKind::Constructor))?;
        rows_while(&view, id, |children, eclass, subsumed| {
            f(Enode {
                name,
                children,
                eclass,
                subsumed,
            })
        });
        Ok(())
    }

    /// The rows of the constructor `name` whose e-class cell is `eclass`.
    fn constructor_enodes_for_eclass(
        &self,
        name: &str,
        eclass: Value,
        mut f: impl FnMut(Enode<'_>),
    ) -> Result<(), Error> {
        self.constructor_enodes(name, |enode| {
            if enode.eclass == eclass {
                f(enode);
            }
        })
    }

    /// The rows of every constructor to an Eq sort whose e-class is `eclass`.
    fn eclass_enodes(&self, eclass: Value, mut f: impl FnMut(Enode<'_>)) -> Result<(), Error> {
        let view = self.view();
        let names: Vec<&str> = (view.tables.iter())
            .filter(|t| t.schema.kind == TableKind::Constructor && !t.schema.name.starts_with('@'))
            .map(|t| t.schema.name.as_str())
            .collect();
        names
            .into_iter()
            .try_for_each(|name| self.constructor_enodes_for_eclass(name, eclass, &mut f))
    }

    fn function_entries(
        &self,
        name: &str,
        mut f: impl FnMut(FunctionEntry<'_>),
    ) -> Result<(), Error> {
        self.function_entries_while(name, |entry| {
            f(entry);
            true
        })
    }

    fn function_entries_while(
        &self,
        name: &str,
        mut f: impl FnMut(FunctionEntry<'_>) -> bool,
    ) -> Result<(), Error> {
        let view = self.view();
        let (id, _) = table(&view, name, Some(TableKind::Function))?;
        rows_while(&view, id, |inputs, output, subsumed| {
            f(FunctionEntry {
                inputs,
                output,
                subsumed,
            })
        });
        Ok(())
    }
}

/// Writes by table name, in [`WriteState`] and [`FullState`]. They apply at
/// the next rebuild: a read of the same state does not see them.
#[allow(private_bounds)]
pub trait Write<'a, 'db: 'a>: Core<'a, 'db> {
    /// The output of a function for `key`, as `(set (name key...) value)`.
    fn set<K: IntoValues, V: IntoValue>(
        &mut self,
        name: &str,
        key: K,
        value: V,
    ) -> Result<(), Error> {
        let (id, mut row) = keyed(&self.view(), name, Some(TableKind::Function), key)?;
        row.push(value.into_value(self.base_values()));
        self.ctx().insert(id, &row);
        Ok(())
    }

    /// The e-class of a constructor row, made if missing; a relation gets
    /// the row.
    fn add<R: IntoValues>(&mut self, name: &str, inputs: R) -> Result<Value, Error> {
        let (id, key) = keyed(&self.view(), name, Some(TableKind::Constructor), inputs)?;
        Ok(self
            .ctx()
            .entry(id, &key)
            .expect("a constructor or relation makes rows"))
    }

    fn remove<K: IntoValues>(&mut self, name: &str, key: K) -> Result<(), Error> {
        let (id, key) = keyed(&self.view(), name, None, key)?;
        self.ctx().remove(id, &key);
        Ok(())
    }

    /// Hides a row from rules; a missing constructor row is made subsumed,
    /// with the e-class a later [`Write::add`] of the key gets.
    fn subsume<K: IntoValues>(&mut self, name: &str, key: K) -> Result<(), Error> {
        let (id, key) = keyed(&self.view(), name, None, key)?;
        self.ctx().subsume(id, &key);
        Ok(())
    }

    fn union(&mut self, x: Value, y: Value) -> Result<(), Error> {
        self.ctx().union(x, y);
        Ok(())
    }

    /// Fails the running rules. Returns `None` to propagate with `?`.
    fn panic(&mut self) -> Option<()> {
        self.ctx().panic();
        None
    }
}

macro_rules! states {
    ($($(#[$doc:meta])* $state:ident: [$($context:ident),+] $(, $extra:ident)*;)+) => {$(
        $(#[$doc])*
        pub struct $state<'a, 'db> {
            pub(crate) ctx: &'a mut PrimCtx<'db>,
        }

        impl<'a, 'db> $state<'a, 'db> {
            pub(crate) fn new(ctx: &'a mut PrimCtx<'db>) -> Self {
                Self { ctx }
            }

            /// The contexts a primitive with this state may run in.
            pub const fn valid_contexts() -> &'static [Context] {
                &[$(Context::$context),+]
            }
        }

        #[allow(private_interfaces)]
        impl<'db> Internal<'db> for $state<'_, 'db> {
            fn view(&self) -> View<'_> {
                self.ctx.view()
            }

            fn ctx(&mut self) -> &mut PrimCtx<'db> {
                self.ctx
            }

            fn stop_flag(&self) -> Option<&Cell<bool>> {
                Some(&self.ctx.stop)
            }
        }

        impl<'a, 'db: 'a> Core<'a, 'db> for $state<'a, 'db> {}
        $(impl<'a, 'db: 'a> $extra<'a, 'db> for $state<'a, 'db> {})*
    )+};
}

states! {
    /// The state of a pure primitive: [`Core`] only.
    PureState: [Pure, Write, Read, Full];
    /// The state of a rule action: [`Core`] and [`Write`].
    WriteState: [Write, Full], Write;
    /// The state of [`EGraph::update`](crate::EGraph::update) and naive rule
    /// actions: [`Core`], [`Read`] and [`Write`].
    FullState: [Full], Read, Write;
}

/// The state of a reader: [`Core`] and [`Read`]. One from
/// [`EGraph::read`](crate::EGraph::read) sees the database but cannot change
/// it, so interning a container there panics.
pub struct ReadState<'a, 'db> {
    source: Source<'a, 'db>,
}

enum Source<'a, 'db> {
    View(View<'a>, PhantomData<&'db ()>),
    Ctx(&'a mut PrimCtx<'db>),
}

impl<'a, 'db> ReadState<'a, 'db> {
    pub(crate) fn new(ctx: &'a mut PrimCtx<'db>) -> Self {
        Self {
            source: Source::Ctx(ctx),
        }
    }

    pub(crate) fn of_view(view: View<'a>) -> Self {
        Self {
            source: Source::View(view, PhantomData),
        }
    }

    /// The contexts a primitive with this state may run in.
    pub const fn valid_contexts() -> &'static [Context] {
        &[Context::Read, Context::Full]
    }
}

#[allow(private_interfaces)]
impl<'db> Internal<'db> for ReadState<'_, 'db> {
    fn view(&self) -> View<'_> {
        match &self.source {
            Source::View(view, _) => *view,
            Source::Ctx(ctx) => ctx.view(),
        }
    }

    fn ctx(&mut self) -> &mut PrimCtx<'db> {
        match &mut self.source {
            Source::Ctx(ctx) => ctx,
            Source::View(..) => panic!("a ReadState of EGraph::read cannot change the database"),
        }
    }

    fn stop_flag(&self) -> Option<&Cell<bool>> {
        match &self.source {
            Source::Ctx(ctx) => Some(&ctx.stop),
            Source::View(..) => None,
        }
    }
}

impl<'a, 'db: 'a> Core<'a, 'db> for ReadState<'a, 'db> {}
impl<'a, 'db: 'a> Read<'a, 'db> for ReadState<'a, 'db> {}
