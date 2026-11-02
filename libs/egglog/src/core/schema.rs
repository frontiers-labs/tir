use std::any::TypeId;
use std::fmt;

use super::{BaseValue, ContainerValue, Database, ExternId, SortId, TableId, Value};
use crate::ast::{Fact, Span};
use crate::{ApiError, ArcSort};

/// What the values of a sort are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKind {
    /// E-class ids from the union-find. A rebuild canonicalizes them.
    Eq,
    /// Values of one [`BaseValue`] type.
    Base(TypeId),
    /// Hash-consed values of one [`ContainerValue`] type. Their ids share
    /// the union-find with e-class ids, so a rebuild canonicalizes them too.
    Container(TypeId),
}

impl SortKind {
    /// The kind of a sort of `T` values.
    pub fn base<T: BaseValue>() -> Self {
        Self::Base(TypeId::of::<T>())
    }

    /// The kind of a sort of `C` containers.
    pub fn container<C: ContainerValue>() -> Self {
        Self::Container(TypeId::of::<C>())
    }

    pub(crate) fn has_ids(self) -> bool {
        !matches!(self, Self::Base(_))
    }
}

/// A hint about the values of one column, which picks the layout of the key
/// map and of the indexes that start with that column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Ids handed out in increasing order, such as e-class ids. The default
    /// for Eq columns.
    Dense,
    /// Any value. The default for base and container columns.
    Sparse,
}

/// How a function combines the output it holds for a key with a new one.
#[derive(Clone, Debug, PartialEq)]
pub enum Merge {
    /// The outputs must be equal, or the write fails.
    AssertEq,
    /// Union the outputs; the older e-class stays.
    Union,
    /// The output the table holds.
    Old,
    /// The output being written.
    New,
    /// This value.
    Const(Value),
    /// A primitive applied to the merges of its arguments. A primitive that
    /// returns `None` fails the write.
    Prim(ExternId, Vec<Merge>),
    /// The output of a table for the merges of its key. A missing row of a
    /// constructor gets a fresh id; of any other table it fails the write.
    Function(TableId, Vec<Merge>),
}

/// The three kinds of tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableKind {
    /// Every column is key; there is no output.
    Relation,
    /// Key columns and one output column, combined by a [`Merge`].
    Function,
    /// A function to an Eq sort whose merge is union; a missing row that is
    /// asked for gets a fresh id.
    Constructor,
}

/// The sorts a primitive takes and returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub inputs: Vec<SortId>,
    pub output: SortId,
}

/// A declared sort.
#[derive(Clone)]
pub struct SortSchema {
    pub(crate) name: String,
    pub(crate) kind: SortKind,
    pub(crate) sort: ArcSort,
}

impl SortSchema {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> SortKind {
        self.kind
    }

    /// How the values of the sort print and what they hold.
    pub fn arcsort(&self) -> &ArcSort {
        &self.sort
    }
}

/// A declared table.
#[derive(Clone)]
pub struct TableSchema {
    pub(crate) name: String,
    pub(crate) kind: TableKind,
    pub(crate) columns: Vec<SortId>,
    pub(crate) keys: usize,
    pub(crate) shapes: Vec<Shape>,
    pub(crate) merge: Option<Merge>,
    // The columns that hold union-find ids, which a rebuild canonicalizes.
    pub(crate) ids: Vec<usize>,
    pub(crate) symmetries: Vec<Vec<usize>>,
    pub(crate) cost: Option<u64>,
    pub(crate) unextractable: bool,
    pub(crate) default: Option<Value>,
}

impl TableSchema {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> TableKind {
        self.kind
    }

    /// The sorts of the key columns, then of the output column if any.
    pub fn columns(&self) -> &[SortId] {
        &self.columns
    }

    pub fn key_arity(&self) -> usize {
        self.keys
    }

    pub fn shapes(&self) -> &[Shape] {
        &self.shapes
    }

    /// `None` for a relation.
    pub fn merge(&self) -> Option<&Merge> {
        self.merge.as_ref()
    }

    /// The column orders other than the identity that leave the set of rows
    /// unchanged: with `p` here, a row `r` is in the table exactly when
    /// `(r[p[0]], r[p[1]], ...)` is. They form a group, and a write of a row
    /// writes each of its orders.
    pub fn symmetries(&self) -> &[Vec<usize>] {
        &self.symmetries
    }

    /// The cost of one row of a constructor in extraction, if set.
    pub fn cost(&self) -> Option<u64> {
        self.cost
    }

    /// Whether extraction skips the rows of this table.
    pub fn is_unextractable(&self) -> bool {
        self.unextractable
    }

    /// The output a function writes for a key that is asked for and missing;
    /// `None` means such a lookup fails.
    pub fn default_value(&self) -> Option<Value> {
        self.default
    }
}

// Adds `order` to the permutation group `group`, which holds every element
// but the identity, and closes it under composition. Returns whether the
// group grew.
pub(crate) fn close_group(group: &mut Vec<Vec<usize>>, order: &[usize]) -> bool {
    let identity: Vec<usize> = (0..order.len()).collect();
    if order == identity || group.iter().any(|p| p == order) {
        return false;
    }
    group.push(order.to_vec());
    let mut known = 0;
    while known < group.len() {
        known = group.len();
        let products: Vec<Vec<usize>> = group
            .iter()
            .flat_map(|a| group.iter().map(|b| b.iter().map(|&c| a[c]).collect()))
            .collect();
        for p in products {
            if p != identity && !group.contains(&p) {
                group.push(p);
            }
        }
    }
    true
}

/// The declarations of a [`Database`]: sorts, tables and primitives.
#[derive(Clone, Copy)]
pub struct Schema<'a> {
    pub(crate) db: &'a Database,
}

impl<'a> Schema<'a> {
    pub fn sort(self, sort: SortId) -> &'a SortSchema {
        &self.db.sorts[sort.index()]
    }

    pub fn sort_named(self, name: &str) -> Option<SortId> {
        self.db.sort_names.get(name).copied()
    }

    pub fn sorts(self) -> impl Iterator<Item = (SortId, &'a SortSchema)> {
        self.db
            .sorts
            .iter()
            .enumerate()
            .map(|(i, sort)| (SortId::new(i), sort))
    }

    pub fn table(self, table: TableId) -> &'a TableSchema {
        &self.db.tables[table.index()].schema
    }

    pub fn table_named(self, name: &str) -> Option<TableId> {
        self.db.table_names.get(name).copied()
    }

    pub fn tables(self) -> impl Iterator<Item = (TableId, &'a TableSchema)> {
        let tables = self.db.tables.iter().enumerate();
        tables.map(|(i, table)| (TableId::new(i), &table.schema))
    }

    /// The name and signature of a primitive.
    pub fn primitive(self, prim: ExternId) -> (&'a str, &'a Signature) {
        let prim = &self.db.prims[prim.index()];
        (&prim.name, &prim.signature)
    }

    /// Where rules may call a primitive.
    pub fn primitive_contexts(self, prim: ExternId) -> &'a [crate::Context] {
        self.db.prims[prim.index()].contexts
    }

    /// Every primitive with this name, one per overload.
    pub fn primitives(self, name: &'a str) -> impl Iterator<Item = ExternId> + 'a {
        let prims = self.db.prims.iter().enumerate();
        prims
            .filter(move |(_, prim)| prim.name == name)
            .map(|(i, _)| ExternId::new(i))
    }
}

/// Why a [`Database`] call failed.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// The merge of a function rejected two outputs for one key: an
    /// [`Merge::AssertEq`] on unequal outputs, a primitive that returned
    /// `None`, or a missing row of a [`Merge::Function`].
    Conflict {
        table: String,
        old: Value,
        new: Value,
    },
    /// No sort or table has this name.
    UnknownName(String),
    /// A row or key has the wrong number of columns.
    Arity {
        table: String,
        expected: usize,
        found: usize,
    },
    /// An Eq or container column got a value that is no id.
    Sort {
        table: String,
        column: usize,
        value: Value,
    },
    /// `set_shapes` got the wrong number of shapes.
    Shape {
        table: String,
        expected: usize,
        found: usize,
    },
    /// A function has no output for a key that was asked for.
    Missing {
        table: String,
    },
    /// A rule does not type: a sort mismatch, an unbound variable, or a call
    /// with no single overload.
    Type(Span, String),
    /// A command has no meaning, such as a `:subsume` rewrite of a variable.
    Desugar(Span, String),
    /// A rule reached a `panic` action.
    Panic(String),
    /// The operation needs what this API does not offer.
    Unsupported(String),
    /// A misuse of a table that the Rust API caught at run time.
    ApiError(ApiError),
    /// A `check` found no match of these facts.
    CheckError(Vec<Fact>, Span),
    NoSuchRuleset(String, Span),
    /// A rule was added to a combined ruleset, which only names others.
    CombinedRulesetError(String, Span),
    RuleAlreadyExists(String, Span),
    CommandAlreadyExists(String, Span),
    /// A `pop` with no `push` before it.
    Pop(Span),
    /// The command inside a `fail` succeeded.
    ExpectFail(Span),
    ExtractError(String),
    /// Reading or writing a file failed.
    Io(String),
    /// A program file did not load.
    Load(crate::LoadError),
    /// A variable or declaration reuses the name of a sort, table, ruleset
    /// or global, declared at the first span.
    Shadowing(String, Span, Span),
    /// A rule subsumes a row of a function, which only constructors allow.
    SubsumeMergeError(String, Span),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { table, old, new } => {
                write!(f, "merge of `{table}` rejects {old:?} and {new:?}")
            }
            Self::UnknownName(name) => write!(f, "no sort or table named `{name}`"),
            Self::Arity {
                table,
                expected,
                found,
            } => {
                write!(f, "`{table}` takes {expected} columns, got {found}")
            }
            Self::Sort {
                table,
                column,
                value,
            } => {
                write!(f, "column {column} of `{table}` holds ids, got {value:?}")
            }
            Self::Shape {
                table,
                expected,
                found,
            } => {
                write!(f, "`{table}` has {expected} columns, got {found} shapes")
            }
            Self::Missing { table } => write!(f, "`{table}` has no row for the key"),
            Self::Type(span, message) => write!(f, "{span}: type error: {message}"),
            Self::Desugar(span, message) => write!(f, "{span}: {message}"),
            Self::Panic(message) => write!(f, "panic: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported: {message}"),
            Self::ApiError(error) => error.fmt(f),
            Self::CheckError(facts, span) => {
                write!(f, "{span}: check failed:")?;
                facts.iter().try_for_each(|fact| write!(f, "\n{fact}"))
            }
            Self::NoSuchRuleset(name, span) => write!(f, "{span}: no such ruleset: {name}"),
            Self::CombinedRulesetError(name, span) => {
                write!(f, "{span}: combined ruleset {name} takes no rules")
            }
            Self::RuleAlreadyExists(name, span) => write!(f, "{span}: rule already exists: {name}"),
            Self::CommandAlreadyExists(name, span) => {
                write!(f, "{span}: command already exists: {name}")
            }
            Self::Pop(span) => write!(f, "{span}: tried to pop too much"),
            Self::ExpectFail(span) => write!(f, "{span}: command should have failed"),
            Self::ExtractError(message) => write!(f, "extraction failure: {message}"),
            Self::Io(message) => write!(f, "IO error: {message}"),
            Self::Load(error) => error.fmt(f),
            Self::Shadowing(name, old, new) => {
                write!(
                    f,
                    "{old}\n{new}\nShadowing is not allowed, but found {name}"
                )
            }
            Self::SubsumeMergeError(name, span) => {
                write!(f, "{span}\nCannot subsume function with merge: {name}")
            }
        }
    }
}

impl std::error::Error for Error {}
