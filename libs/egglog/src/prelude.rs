// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/prelude.rs. See libs/egglog/LICENSE-MIT.

//! The Rust entry point of egglog: macros that write egglog syntax as Rust
//! tokens, and functions that declare tables and rules on an [`EGraph`].
//!
//! ```text
//! use tir_egglog::prelude::*;
//! let mut egraph = EGraph::default();
//! datatype!(&mut egraph, (datatype Math (Add Math Math) (Num i64)));
//! rule(&mut egraph, "", facts![(= e (Add a b))], actions![(union e (Add b a))])?;
//! ```

use std::any::{Any, TypeId};
use std::fmt::Debug;
use std::sync::Arc;

use crate::ast::{Command, RuleEvalMode, RunConfig, Schedule, Schema};
use crate::extract::DefaultCost;
use crate::sort::Sort;
use crate::{
    ArcSort, BaseValue, BaseValues, CommandOutput, ContainerValue, ContainerValues, Error, F64,
    PrimFn, TermDag, TermId, Value,
};

pub use crate::ast::{Action, Actions, Change, Expr, Fact, Facts, GenericActions, Literal, Span};
pub use crate::sort::{BigIntSort, BigRatSort, BoolSort, F64Sort, I64Sort, StringSort, UnitSort};
pub use crate::{CommandMacro, CommandMacroRegistry, EGraph};
pub use crate::{Core, FullState, PureState, Read, ReadState, Write, WriteState};
pub use crate::{action, actions, datatype, expr, fact, facts, sort, span, vars};

/// A Rust type that a literal holds.
pub trait LiteralConvertible: Sized {
    fn to_literal(self) -> Literal;
    fn from_literal(lit: &Literal) -> Option<Self>;
}

macro_rules! literal_convertible {
    ($($ty:ty: $variant:ident),*) => {$(
        impl LiteralConvertible for $ty {
            fn to_literal(self) -> Literal {
                Literal::$variant(self)
            }

            fn from_literal(lit: &Literal) -> Option<Self> {
                match lit {
                    Literal::$variant(x) => Some(x.clone()),
                    _ => None,
                }
            }
        }
    )*};
}

literal_convertible!(i64: Int, bool: Bool, F64: Float, String: String);

impl LiteralConvertible for () {
    fn to_literal(self) -> Literal {
        Literal::Unit
    }

    fn from_literal(lit: &Literal) -> Option<Self> {
        matches!(lit, Literal::Unit).then_some(())
    }
}

pub mod exprs {
    use super::*;

    /// Creates a variable expression.
    pub fn var(name: &str) -> Expr {
        Expr::Var(span!(), name.to_owned())
    }

    /// Creates an integer literal expression.
    pub fn int(value: i64) -> Expr {
        Expr::Lit(span!(), Literal::Int(value))
    }

    /// Creates a float literal expression.
    pub fn float(value: f64) -> Expr {
        Expr::Lit(span!(), value.into())
    }

    /// Creates a string literal expression.
    pub fn string(value: &str) -> Expr {
        Expr::Lit(span!(), Literal::String(value.to_owned()))
    }

    /// Creates a unit literal expression.
    pub fn unit() -> Expr {
        Expr::Lit(span!(), Literal::Unit)
    }

    /// Creates a boolean literal expression.
    pub fn bool(value: bool) -> Expr {
        Expr::Lit(span!(), Literal::Bool(value))
    }

    /// Creates a function call expression.
    pub fn call(f: &str, xs: Vec<Expr>) -> Expr {
        Expr::Call(span!(), f.to_owned(), xs)
    }
}

/// The [`Span`] of the line that expands it.
#[macro_export]
macro_rules! span {
    () => {
        $crate::ast::Span {
            file: file!(),
            line: line!(),
        }
    };
}

#[macro_export]
macro_rules! sort {
    (BigInt) => {
        BigIntSort.to_arcsort()
    };
    (BigRat) => {
        BigRatSort.to_arcsort()
    };
    (bool) => {
        BoolSort.to_arcsort()
    };
    (f64) => {
        F64Sort.to_arcsort()
    };
    (i64) => {
        I64Sort.to_arcsort()
    };
    (String) => {
        StringSort.to_arcsort()
    };
    (Unit) => {
        UnitSort.to_arcsort()
    };
    ($t:expr) => {
        $t
    };
}

#[macro_export]
macro_rules! vars {
    [$($x:ident : $t:tt),* $(,)?] => {
        &[$((stringify!($x), sort!($t))),*]
    };
}

#[macro_export]
macro_rules! expr {
    ((unquote $unquoted:expr)) => { $unquoted };
    (($func:tt $($arg:tt)*)) => { exprs::call(stringify!($func), vec![$(expr!($arg)),*]) };
    ($value:literal) => { exprs::int($value) };
    ($quoted:tt) => { exprs::var(stringify!($quoted)) };
}

#[macro_export]
macro_rules! fact {
    ((= $($arg:tt)*)) => { Fact::Eq(span!(), $(expr!($arg)),*) };
    ($a:tt) => { Fact::Fact(expr!($a)) };
}

#[macro_export]
macro_rules! facts {
    ($($tree:tt)*) => { Facts(vec![$(fact!($tree)),*]) };
}

#[macro_export]
macro_rules! action {
    ((let $name:ident $value:tt)) => {
        Action::Let(span!(), String::from(stringify!($name)), expr!($value))
    };
    ((set ($f:ident $($x:tt)*) $value:tt)) => {
        Action::Set(span!(), String::from(stringify!($f)), vec![$(expr!($x)),*], expr!($value))
    };
    ((delete ($f:ident $($x:tt)*))) => {
        Action::Change(span!(), Change::Delete, String::from(stringify!($f)), vec![$(expr!($x)),*])
    };
    ((subsume ($f:ident $($x:tt)*))) => {
        Action::Change(span!(), Change::Subsume, String::from(stringify!($f)), vec![$(expr!($x)),*])
    };
    ((union $x:tt $y:tt)) => {
        Action::Union(span!(), expr!($x), expr!($y))
    };
    ((panic $message:literal)) => {
        Action::Panic(span!(), $message.to_owned())
    };
    ($x:tt) => {
        Action::Expr(span!(), expr!($x))
    };
}

#[macro_export]
macro_rules! actions {
    ($($tree:tt)*) => { GenericActions(vec![$(action!($tree)),*]) };
}

/// Declares an Eq sort and its constructors:
/// `datatype!(&mut egraph, (datatype Math (Num i64) (Add Math Math :cost 2)))`.
#[macro_export]
macro_rules! datatype {
    ($egraph:expr, (datatype $sort:ident $(($name:ident $($args:ident)* $(:cost $cost:expr)?))*)) => {
        add_sort($egraph, stringify!($sort))?;
        $(add_constructor(
            $egraph,
            stringify!($name),
            $crate::ast::Schema {
                input: vec![$(stringify!($args).to_owned()),*],
                output: stringify!($sort).to_owned(),
            },
            [$($cost)*].first().copied(),
            false,
        )?;)*
    };
}

pub fn add_ruleset(egraph: &mut EGraph, ruleset: &str) -> Result<Vec<CommandOutput>, Error> {
    egraph.run_program(vec![Command::AddRuleset(span!(), ruleset.to_owned())])
}

/// Runs one iteration of a ruleset.
pub fn run_ruleset(egraph: &mut EGraph, ruleset: &str) -> Result<Vec<CommandOutput>, Error> {
    let config = RunConfig {
        ruleset: ruleset.to_owned(),
        until: None,
    };
    egraph.run_program(vec![Command::RunSchedule(Schedule::Run(span!(), config))])
}

/// Adds a rule that runs `actions` for each match of `facts`.
pub fn rule(
    egraph: &mut EGraph,
    ruleset: &str,
    facts: Facts,
    actions: Actions,
) -> Result<Vec<CommandOutput>, Error> {
    let rule = crate::ast::Rule {
        span: span!(),
        head: actions,
        body: facts.0,
        name: String::new(),
        ruleset: ruleset.to_owned(),
        eval_mode: RuleEvalMode::Seminaive,
    };
    egraph.run_program(vec![Command::Rule { rule }])
}

/// Adds a rule whose action calls `func` with the bindings of `vars` in
/// each match of `facts`. A program saved with it needs
/// [`EGraph::bind_rust_rule`] of `rule_name` before it loads.
pub fn rust_rule(
    egraph: &mut EGraph,
    rule_name: &str,
    ruleset: &str,
    vars: &[(&str, ArcSort)],
    facts: Facts,
    func: impl for<'a, 'db> Fn(WriteState<'a, 'db>, &[Value]) -> Option<()>
    + Clone
    + Send
    + Sync
    + 'static,
) -> Result<Vec<CommandOutput>, Error> {
    let f: Arc<PrimFn> = Arc::new(move |ctx, args| {
        func(WriteState::new(ctx), args)?;
        Some(ctx.base(()))
    });
    egraph.rust_rule(rule_name, ruleset, vars, facts, false, f)
}

/// Like [`rust_rule`], but `func` may read the database too, so the rule
/// matches the whole database each run.
pub fn rust_rule_full(
    egraph: &mut EGraph,
    rule_name: &str,
    ruleset: &str,
    vars: &[(&str, ArcSort)],
    facts: Facts,
    func: impl for<'a, 'db> Fn(FullState<'a, 'db>, &[Value]) -> Option<()>
    + Clone
    + Send
    + Sync
    + 'static,
) -> Result<Vec<CommandOutput>, Error> {
    let f: Arc<PrimFn> = Arc::new(move |ctx, args| {
        func(FullState::new(ctx), args)?;
        Some(ctx.base(()))
    });
    egraph.rust_rule(rule_name, ruleset, vars, facts, true, f)
}

/// Declares an Eq sort.
pub fn add_sort(egraph: &mut EGraph, name: &str) -> Result<Vec<CommandOutput>, Error> {
    egraph.run_program(vec![Command::Sort {
        span: span!(),
        name: name.to_owned(),
        presort_and_args: None,
    }])
}

/// Declares a function; with no merge, writing a second output is an error.
pub fn add_function(
    egraph: &mut EGraph,
    name: &str,
    schema: Schema,
    merge: Option<Expr>,
) -> Result<Vec<CommandOutput>, Error> {
    egraph.run_program(vec![Command::Function {
        span: span!(),
        name: name.to_owned(),
        schema,
        merge,
    }])
}

pub fn add_constructor(
    egraph: &mut EGraph,
    name: &str,
    schema: Schema,
    cost: Option<DefaultCost>,
    unextractable: bool,
) -> Result<Vec<CommandOutput>, Error> {
    egraph.run_program(vec![Command::Constructor {
        span: span!(),
        name: name.to_owned(),
        schema,
        cost,
        unextractable,
    }])
}

pub fn add_relation(
    egraph: &mut EGraph,
    name: &str,
    inputs: Vec<String>,
) -> Result<Vec<CommandOutput>, Error> {
    egraph.run_program(vec![Command::Relation {
        span: span!(),
        name: name.to_owned(),
        inputs,
    }])
}

/// A sort of plain Rust values. Register it with [`add_base_sort`].
pub trait BaseSort: Any + Send + Sync + Debug {
    type Base: BaseValue;
    fn name(&self) -> &str;
    fn register_primitives(&self, _eg: &mut EGraph) {}
    fn reconstruct_termdag(&self, base: &BaseValues, value: Value, termdag: &mut TermDag)
    -> TermId;

    fn to_arcsort(self) -> ArcSort
    where
        Self: Sized,
    {
        Arc::new(BaseSortImpl(self))
    }
}

#[derive(Debug)]
struct BaseSortImpl<T>(T);

impl<T: BaseSort> Sort for BaseSortImpl<T> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }

    fn value_type(&self) -> Option<TypeId> {
        Some(TypeId::of::<T::Base>())
    }

    fn register_primitives(self: Arc<Self>, eg: &mut EGraph) {
        self.0.register_primitives(eg);
    }

    fn reconstruct_termdag_base(
        &self,
        base: &BaseValues,
        value: Value,
        termdag: &mut TermDag,
    ) -> TermId {
        self.0.reconstruct_termdag(base, value, termdag)
    }
}

/// A sort of containers of a Rust type. Register it with
/// [`add_container_sort`].
pub trait ContainerSort: Any + Send + Sync + Debug {
    type Container: ContainerValue;
    fn name(&self) -> &str;
    fn is_eq_container_sort(&self) -> bool;
    fn inner_sorts(&self) -> Vec<ArcSort>;
    fn inner_values(&self, containers: &ContainerValues, value: Value) -> Vec<(ArcSort, Value)>;
    fn register_primitives(&self, _eg: &mut EGraph) {}
    fn reconstruct_termdag(
        &self,
        containers: &ContainerValues,
        value: Value,
        termdag: &mut TermDag,
        element_terms: Vec<TermId>,
    ) -> TermId;
    fn serialized_name(&self, containers: &ContainerValues, value: Value) -> String;

    fn to_arcsort(self) -> ArcSort
    where
        Self: Sized,
    {
        Arc::new(ContainerSortImpl(self))
    }
}

#[derive(Debug)]
struct ContainerSortImpl<T>(T);

impl<T: ContainerSort> Sort for ContainerSortImpl<T> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn inner_sorts(&self) -> Vec<ArcSort> {
        self.0.inner_sorts()
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }

    fn is_container_sort(&self) -> bool {
        true
    }

    fn is_eq_container_sort(&self) -> bool {
        self.0.is_eq_container_sort()
    }

    fn serialized_name(&self, containers: &ContainerValues, value: Value) -> String {
        self.0.serialized_name(containers, value)
    }

    fn inner_values(&self, containers: &ContainerValues, value: Value) -> Vec<(ArcSort, Value)> {
        self.0.inner_values(containers, value)
    }

    fn value_type(&self) -> Option<TypeId> {
        Some(TypeId::of::<T::Container>())
    }

    fn register_primitives(self: Arc<Self>, eg: &mut EGraph) {
        self.0.register_primitives(eg);
    }

    fn reconstruct_termdag_container(
        &self,
        containers: &ContainerValues,
        value: Value,
        termdag: &mut TermDag,
        element_terms: Vec<TermId>,
    ) -> TermId {
        self.0
            .reconstruct_termdag(containers, value, termdag, element_terms)
    }
}

pub fn add_base_sort(
    egraph: &mut EGraph,
    base_sort: impl BaseSort,
    span: Span,
) -> Result<(), Error> {
    egraph.add_arcsort(base_sort.to_arcsort(), span)
}

pub fn add_container_sort(
    egraph: &mut EGraph,
    container_sort: impl ContainerSort,
    span: Span,
) -> Result<(), Error> {
    egraph.add_arcsort(container_sort.to_arcsort(), span)
}
