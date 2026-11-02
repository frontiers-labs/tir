// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/sort/mod.rs and src/sort/{i64,f64,bool,string,unit}.rs. See libs/egglog/LICENSE-MIT.

//! Sorts: what the values of a column are, how they print, and the
//! primitives on them.

mod bigint;
mod containers;

use std::any::{Any, TypeId};
use std::fmt::Debug;
use std::sync::Arc;

pub use bigint::{BigInt, BigIntSort, BigRat, BigRatSort};
pub use containers::{
    Collection, Container, ContainerKind, FunctionContainer, FunctionSort, MapContainer, MapKind,
    MapSort, MultiSetContainer, MultiSetKind, MultiSetSort, PairContainer, PairKind, PairSort,
    SetContainer, SetKind, SetSort, VecContainer, VecKind, VecSort,
};
pub(crate) use containers::{FN_PREFIX, register_functions};

use crate::ast::{Expr, Literal, Span};
use crate::prelude::BaseSort;
use crate::vm::Const;
use crate::{
    ArcSort, BaseValues, ContainerValues, Database, EGraph, Error, F64, SortId, SortKind, TermDag,
    TermId, Value,
};

pub type F = F64;
pub type S = String;
pub type Z = BigInt;
pub type Q = BigRat;

/// A sort (type) of values. Implement [`BaseSort`] or
/// [`ContainerSort`](crate::prelude::ContainerSort) rather than this trait
/// for a sort of plain values or of containers.
pub trait Sort: Any + Send + Sync + Debug {
    fn name(&self) -> &str;

    /// The sorts one level inside a container sort.
    fn inner_sorts(&self) -> Vec<ArcSort> {
        Vec::new()
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static>;

    fn is_eq_sort(&self) -> bool {
        false
    }

    fn is_container_sort(&self) -> bool {
        false
    }

    /// Whether the containers of this sort hold e-class ids, directly or in
    /// containers inside, so a rebuild canonicalizes them.
    fn is_eq_container_sort(&self) -> bool {
        false
    }

    fn serialized_name(&self, _container_values: &ContainerValues, _value: Value) -> String {
        self.name().to_owned()
    }

    /// The values inside a container of this sort, with their sorts.
    fn inner_values(
        &self,
        _container_values: &ContainerValues,
        _value: Value,
    ) -> Vec<(ArcSort, Value)> {
        Vec::new()
    }

    /// The Rust type of the values; `None` for an Eq sort.
    fn value_type(&self) -> Option<TypeId>;

    fn register_primitives(self: Arc<Self>, eg: &mut EGraph) {
        let _ = eg;
    }

    /// The term of a container whose elements extracted to `element_terms`.
    fn reconstruct_termdag_container(
        &self,
        _container_values: &ContainerValues,
        _value: Value,
        termdag: &mut TermDag,
        element_terms: Vec<TermId>,
    ) -> TermId {
        termdag.app(self.name().to_owned(), element_terms)
    }

    /// The term of a value with no parts, such as a literal. An opaque
    /// value prints as `sort#cell`.
    fn reconstruct_termdag_base(
        &self,
        _base_values: &BaseValues,
        value: Value,
        termdag: &mut TermDag,
    ) -> TermId {
        termdag.var(format!("{}#{}", self.name(), value.raw()))
    }
}

// How a database stores the values of `sort`.
pub(crate) fn kind(sort: &dyn Sort) -> SortKind {
    match (
        sort.is_eq_sort(),
        sort.is_container_sort(),
        sort.value_type(),
    ) {
        (true, ..) | (_, _, None) => SortKind::Eq,
        (false, true, Some(ty)) => SortKind::Container(ty),
        (false, false, Some(ty)) => SortKind::Base(ty),
    }
}

/// A sort constructor such as `Vec`, which `(sort Name (Vec Elem))` applies.
pub trait Presort {
    fn presort_name() -> &'static str;
    fn reserved_primitives() -> Vec<&'static str>;
    fn make_sort(db: &Database, name: String, args: &[Expr], span: Span) -> Result<ArcSort, Error>;
}

pub type MkSort = fn(&Database, String, &[Expr], Span) -> Result<ArcSort, Error>;

/// A sort of e-class ids.
#[derive(Debug)]
pub struct EqSort {
    pub name: String,
}

impl Sort for EqSort {
    fn name(&self) -> &str {
        &self.name
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }

    fn is_eq_sort(&self) -> bool {
        true
    }

    fn value_type(&self) -> Option<TypeId> {
        None
    }
}

// A base or container sort declared on a database with no Rust sort to
// print its values.
#[derive(Debug)]
pub(crate) struct OpaqueSort {
    pub(crate) name: String,
    pub(crate) kind: SortKind,
}

impl Sort for OpaqueSort {
    fn name(&self) -> &str {
        &self.name
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }

    fn is_container_sort(&self) -> bool {
        matches!(self.kind, SortKind::Container(_))
    }

    fn value_type(&self) -> Option<TypeId> {
        match self.kind {
            SortKind::Base(ty) | SortKind::Container(ty) => Some(ty),
            SortKind::Eq => None,
        }
    }
}

/// The sort of a literal.
pub fn literal_sort(literal: &Literal) -> ArcSort {
    match literal {
        Literal::Int(_) => I64Sort.to_arcsort(),
        Literal::Float(_) => F64Sort.to_arcsort(),
        Literal::String(_) => StringSort.to_arcsort(),
        Literal::Bool(_) => BoolSort.to_arcsort(),
        Literal::Unit => UnitSort.to_arcsort(),
        Literal::Arg(_) => unreachable!("a run argument has no sort of its own"),
    }
}

/// Registers one overload of a primitive whose arguments and result convert
/// through [`FromValue`](crate::FromValue) and [`IntoValue`](crate::IntoValue).
/// The body returns an `Option`; `None` means no result. Name the context
/// before a `;` to use it in the body, and the contexts the primitive may
/// run in after `in`; by default it runs anywhere.
macro_rules! prim {
    ($db:expr, $name:expr, [$($input:expr),*] -> $output:expr, in $contexts:expr,
     |$ctx:ident; $($arg:ident: $ty:ty),*| $body:expr) => {
        $db.add_primitive_in(
            $name,
            $crate::Signature { inputs: vec![$($input),*], output: $output },
            $contexts,
            move |$ctx, args| {
                let &[$($arg),*] = args else { unreachable!("the signature fixes the arity") };
                $(let $arg = <$ty as $crate::FromValue>::from_value($arg, $ctx.base_values());)*
                let out = $body?;
                Some($crate::IntoValue::into_value(out, $ctx.base_values()))
            },
        )
    };
    ($db:expr, $name:expr, [$($input:expr),*] -> $output:expr,
     |$ctx:ident; $($arg:ident: $ty:ty),*| $body:expr) => {
        prim!($db, $name, [$($input),*] -> $output, in &$crate::Context::ALL, |$ctx; $($arg: $ty),*| $body)
    };
    ($db:expr, $name:expr, [$($input:expr),*] -> $output:expr, |$($arg:ident: $ty:ty),*| $body:expr) => {
        prim!($db, $name, [$($input),*] -> $output, |ctx; $($arg: $ty),*| $body)
    };
}
pub(crate) use prim;

// Variadic primitives get one overload per arity up to this.
pub(crate) const VARIADIC: usize = 8;

fn sorts(db: &Database, names: [&str; 5]) -> [SortId; 5] {
    names.map(|name| db.sort(name).expect("a base sort"))
}

macro_rules! base_sort {
    ($sort:ident, $name:literal, $base:ty, $register:ident, |$value:ident| $literal:expr) => {
        #[derive(Debug)]
        pub struct $sort;

        impl BaseSort for $sort {
            type Base = $base;

            fn name(&self) -> &str {
                $name
            }

            fn register_primitives(&self, eg: &mut EGraph) {
                $register(eg.database_mut());
            }

            fn reconstruct_termdag(
                &self,
                base: &BaseValues,
                value: Value,
                termdag: &mut TermDag,
            ) -> TermId {
                let $value: $base = base.unwrap(value);
                termdag.lit($literal)
            }
        }
    };
}

base_sort!(I64Sort, "i64", i64, register_i64, |x| Literal::Int(x));
base_sort!(F64Sort, "f64", F64, register_f64, |x| Literal::Float(x));
base_sort!(BoolSort, "bool", bool, register_bool, |x| Literal::Bool(x));
base_sort!(StringSort, "String", String, register_string, |x| {
    Literal::String(x)
});
base_sort!(UnitSort, "Unit", (), register_unit, |_x| Literal::Unit);

fn register_unit(_db: &mut Database) {}

#[rustfmt::skip]
fn register_i64(db: &mut Database) {
    let [i, _, b, s, u] = sorts(db, ["i64", "f64", "bool", "String", "Unit"]);
    prim!(db, "+", [i, i] -> i, |a: i64, b: i64| a.checked_add(b));
    prim!(db, "-", [i, i] -> i, |a: i64, b: i64| a.checked_sub(b));
    prim!(db, "*", [i, i] -> i, |a: i64, b: i64| a.checked_mul(b));
    prim!(db, "/", [i, i] -> i, |a: i64, b: i64| a.checked_div(b));
    prim!(db, "%", [i, i] -> i, |a: i64, b: i64| a.checked_rem(b));
    prim!(db, "&", [i, i] -> i, |a: i64, b: i64| Some(a & b));
    prim!(db, "|", [i, i] -> i, |a: i64, b: i64| Some(a | b));
    prim!(db, "^", [i, i] -> i, |a: i64, b: i64| Some(a ^ b));
    prim!(db, "<<", [i, i] -> i, |a: i64, b: i64| u32::try_from(b).ok().and_then(|b| a.checked_shl(b)));
    prim!(db, ">>", [i, i] -> i, |a: i64, b: i64| u32::try_from(b).ok().and_then(|b| a.checked_shr(b)));
    prim!(db, "not-i64", [i] -> i, |a: i64| Some(!a));
    prim!(db, "log2", [i] -> i, |a: i64| a.checked_ilog2().map(i64::from));
    prim!(db, "abs", [i] -> i, |a: i64| a.checked_abs());
    prim!(db, "<", [i, i] -> u, |a: i64, b: i64| (a < b).then_some(()));
    prim!(db, ">", [i, i] -> u, |a: i64, b: i64| (a > b).then_some(()));
    prim!(db, "<=", [i, i] -> u, |a: i64, b: i64| (a <= b).then_some(()));
    prim!(db, ">=", [i, i] -> u, |a: i64, b: i64| (a >= b).then_some(()));
    prim!(db, "bool-=", [i, i] -> b, |x: i64, y: i64| Some(x == y));
    prim!(db, "bool-<", [i, i] -> b, |x: i64, y: i64| Some(x < y));
    prim!(db, "bool->", [i, i] -> b, |x: i64, y: i64| Some(x > y));
    prim!(db, "bool-<=", [i, i] -> b, |x: i64, y: i64| Some(x <= y));
    prim!(db, "bool->=", [i, i] -> b, |x: i64, y: i64| Some(x >= y));
    prim!(db, "min", [i, i] -> i, |a: i64, b: i64| Some(a.min(b)));
    prim!(db, "max", [i, i] -> i, |a: i64, b: i64| Some(a.max(b)));
    prim!(db, "to-string", [i] -> s, |a: i64| Some(a.to_string()));
    prim!(db, "count-matches", [s, s] -> i, |a: String, b: String| i64::try_from(a.matches(&b).count()).ok());
}

#[rustfmt::skip]
fn register_f64(db: &mut Database) {
    let [i, f, _, s, u] = sorts(db, ["i64", "f64", "bool", "String", "Unit"]);
    prim!(db, "+", [f, f] -> f, |a: f64, b: f64| Some(a + b));
    prim!(db, "-", [f, f] -> f, |a: f64, b: f64| Some(a - b));
    prim!(db, "*", [f, f] -> f, |a: f64, b: f64| Some(a * b));
    prim!(db, "/", [f, f] -> f, |a: f64, b: f64| (b != 0.0).then_some(a / b));
    prim!(db, "%", [f, f] -> f, |a: f64, b: f64| (b != 0.0).then_some(a % b));
    prim!(db, "^", [f, f] -> f, |a: f64, b: f64| Some(a.powf(b)));
    prim!(db, "neg", [f] -> f, |a: f64| Some(-a));
    prim!(db, "<", [f, f] -> u, |a: F64, b: F64| (a < b).then_some(()));
    prim!(db, ">", [f, f] -> u, |a: F64, b: F64| (a > b).then_some(()));
    prim!(db, "<=", [f, f] -> u, |a: F64, b: F64| (a <= b).then_some(()));
    prim!(db, ">=", [f, f] -> u, |a: F64, b: F64| (a >= b).then_some(()));
    prim!(db, "min", [f, f] -> f, |a: F64, b: F64| Some(a.min(b)));
    prim!(db, "max", [f, f] -> f, |a: F64, b: F64| Some(a.max(b)));
    prim!(db, "abs", [f] -> f, |a: f64| Some(a.abs()));
    prim!(db, "exp", [f] -> f, |a: f64| Some(a.exp()));
    prim!(db, "log", [f] -> f, |a: f64| (a > 0.0).then(|| a.ln()));
    prim!(db, "sqrt", [f] -> f, |a: f64| (a >= 0.0).then(|| a.sqrt()));
    prim!(db, "to-f64", [i] -> f, |a: i64| Some(a as f64));
    prim!(db, "to-i64", [f] -> i, |a: f64| Some(a as i64));
    prim!(db, "to-string", [f] -> s, |a: f64| Some(format!("{a:?}")));
}

#[rustfmt::skip]
fn register_bool(db: &mut Database) {
    let [_, _, b, _, u] = sorts(db, ["i64", "f64", "bool", "String", "Unit"]);
    prim!(db, "not", [b] -> b, |a: bool| Some(!a));
    prim!(db, "and", [b, b] -> b, |x: bool, y: bool| Some(x && y));
    prim!(db, "xor", [b, b] -> b, |x: bool, y: bool| Some(x ^ y));
    prim!(db, "=>", [b, b] -> b, |x: bool, y: bool| Some(!x || y));
    prim!(db, "guard", [b] -> u, |a: bool| a.then_some(()));
    for arity in 0..=VARIADIC {
        let signature = crate::Signature { inputs: vec![b; arity], output: b };
        db.add_primitive("or", signature, |ctx, args| {
            Some(ctx.base(args.iter().any(|&x| ctx.base_value::<bool>(x))))
        });
    }
}

#[rustfmt::skip]
fn register_string(db: &mut Database) {
    let [_, _, _, s, _] = sorts(db, ["i64", "f64", "bool", "String", "Unit"]);
    prim!(db, "replace", [s, s, s] -> s, |a: String, b: String, c: String| Some(a.replace(&b, &c)));
    for arity in 0..=VARIADIC {
        let signature = crate::Signature { inputs: vec![s; arity], output: s };
        db.add_primitive("+", signature, |ctx, args| {
            Some(ctx.base(args.iter().map(|&x| ctx.base_value::<String>(x)).collect::<String>()))
        });
    }
}

// The primitives every sort has: comparisons of the cells, and the probe
// whose overload tells the sort of an expression.
#[rustfmt::skip]
pub(crate) fn register_generic(db: &mut Database, x: SortId) {
    let [_, _, b, _, u] = sorts(db, ["i64", "f64", "bool", "String", "Unit"]);
    prim!(db, "!=", [x, x] -> u, |l: Value, r: Value| (l != r).then_some(()));
    prim!(db, "bool-!=", [x, x] -> b, |l: Value, r: Value| Some(l != r));
    prim!(db, "value-eq", [x, x] -> u, |l: Value, r: Value| (l == r).then_some(()));
    prim!(db, "ordering-min", [x, x] -> x, |l: Value, r: Value| Some(l.min(r)));
    prim!(db, "ordering-max", [x, x] -> x, |l: Value, r: Value| Some(l.max(r)));
    prim!(db, PROBE, [x] -> u, |_v: Value| Some(()));
}

pub(crate) const PROBE: &str = "@probe";

// `value` of `sort` as a literal that means the same in any database: base
// values by their Rust value, containers by their elements. E-class ids and
// values of other types have no such literal.
pub(crate) fn constant(db: &Database, sort: SortId, value: Value) -> Result<Const, Error> {
    let schema = db.schema().sort(sort);
    let base = db.base_values();
    let unsupported = || Error::Unsupported(format!("a constant of sort {}", schema.name()));
    let SortKind::Base(ty) = schema.kind() else {
        let elements = schema.arcsort().inner_values(db.container_values(), value);
        if schema.kind() == SortKind::Eq || containers::rebuilt(schema.arcsort()).is_none() {
            return Err(unsupported());
        }
        let elements = elements
            .iter()
            .map(|(s, v)| constant(db, db.sort(s.name())?, *v));
        return Ok(Const::Elems(elements.collect::<Result<_, _>>()?));
    };
    let literal = |literal| Ok(Const::Lit(literal));
    match ty {
        _ if ty == TypeId::of::<i64>() => literal(Literal::Int(base.unwrap(value))),
        _ if ty == TypeId::of::<F64>() => literal(Literal::Float(base.unwrap(value))),
        _ if ty == TypeId::of::<String>() => literal(Literal::String(base.unwrap(value))),
        _ if ty == TypeId::of::<bool>() => literal(Literal::Bool(base.unwrap(value))),
        _ if ty == TypeId::of::<()>() => literal(Literal::Unit),
        _ if ty == TypeId::of::<BigInt>() => {
            Ok(Const::BigInt(base.unwrap::<BigInt>(value).to_string()))
        }
        _ if ty == TypeId::of::<BigRat>() => {
            let rat = base.unwrap::<BigRat>(value);
            Ok(Const::BigRat(
                rat.numer().to_string(),
                rat.denom().to_string(),
            ))
        }
        _ => Err(unsupported()),
    }
}

// The value of a constant of `sort`, interned into `db`; `None` if the
// constant does not fit the sort.
pub(crate) fn intern(db: &mut Database, sort: SortId, value: &Const) -> Option<Value> {
    let arcsort = db.schema().sort(sort).arcsort().clone();
    match value {
        Const::Lit(literal) => {
            let fits = literal_sort(literal).name() == arcsort.name();
            fits.then(|| crate::vm::value(db.base_values(), literal))
        }
        Const::BigInt(digits) => Some(db.base(digits.parse::<BigInt>().ok()?)),
        Const::BigRat(num, den) => {
            Some(db.base(BigRat::new(num.parse().ok()?, den.parse().ok()?)?))
        }
        Const::Elems(elements) => {
            let sorts = arcsort.inner_sorts();
            let sorts = sorts.iter().cycle().map(|s| db.sort(s.name()).ok());
            let pairs: Vec<(Option<SortId>, &Const)> = sorts.zip(elements).collect();
            let data = pairs
                .into_iter()
                .map(|(s, c)| intern(db, s?, c))
                .collect::<Option<_>>()?;
            containers::rebuilt(&arcsort)?(db, data)
        }
    }
}
