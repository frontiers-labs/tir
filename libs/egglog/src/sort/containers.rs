// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/sort/{vec,set,map,multiset,pair,fn}.rs. See libs/egglog/LICENSE-MIT.

//! The container sorts `Vec`, `Set`, `MultiSet`, `Map` and `Pair` share one
//! generic container, [`Container`], whose kind says how its values sit in
//! [`Container::data`]. `UnstableFn` holds a function with some of its
//! arguments given.

use std::any::{Any, TypeId};
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::Arc;

use super::{Presort, Sort, VARIADIC, prim};
use crate::ast::{Expr, Literal, Span};
use crate::{
    ArcSort, ContainerValue, ContainerValues, Database, EGraph, Error, PrimCtx, Signature, SortId,
    TableKind, TermDag, TermId, Value,
};

/// How the values of a container kind sit in [`Container::data`].
pub trait ContainerKind: 'static + Clone + Debug + Default + Hash + Eq + Send + Sync {
    /// The name of the sort constructor, such as `Vec`.
    const PRESORT: &'static str;
    /// The sorts the constructor takes. With two, the data alternates
    /// between them.
    const ARITY: usize;
    /// The head of the term that builds a container.
    const HEAD: &'static str;

    /// Brings `data` into the one order equal containers share.
    fn normalize(data: &mut Vec<Value>) {
        let _ = data;
    }

    fn register(sort: &Collection<Self>, db: &mut Database);
}

/// A container of a [`ContainerKind`]. `ids` says which of the alternating
/// columns of `data` hold e-class ids, which a rebuild canonicalizes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Container<K> {
    pub data: Vec<Value>,
    ids: [bool; 2],
    kind: PhantomData<K>,
}

impl<K: ContainerKind> Container<K> {
    pub fn new(mut data: Vec<Value>, ids: [bool; 2]) -> Self {
        K::normalize(&mut data);
        Self {
            data,
            ids,
            kind: PhantomData,
        }
    }
}

impl<K: ContainerKind> ContainerValue for Container<K> {
    fn rebuild_contents(&mut self, find: &dyn Fn(Value) -> Value) -> bool {
        let mut changed = false;
        for (i, value) in self.data.iter_mut().enumerate() {
            if self.ids[i % 2] {
                let found = find(*value);
                changed |= found != *value;
                *value = found;
            }
        }
        K::normalize(&mut self.data);
        changed
    }

    fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        self.data.iter().copied()
    }
}

macro_rules! kind {
    ($kind:ident, $presort:literal, $arity:literal, $head:literal, $register:ident $(, $normalize:ident)?) => {
        #[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
        pub struct $kind;

        impl ContainerKind for $kind {
            const PRESORT: &'static str = $presort;
            const ARITY: usize = $arity;
            const HEAD: &'static str = $head;
            $(fn normalize(data: &mut Vec<Value>) {
                $normalize(data)
            })?

            fn register(sort: &Collection<Self>, db: &mut Database) {
                $register(sort, db)
            }
        }
    };
}

kind!(VecKind, "Vec", 1, "vec-of", register_vec);
kind!(SetKind, "Set", 1, "set-of", register_set, sorted_set);
kind!(
    MultiSetKind,
    "MultiSet",
    1,
    "multiset-of",
    register_multiset,
    sorted
);
kind!(MapKind, "Map", 2, "map-of", register_map, sorted_map);
kind!(PairKind, "Pair", 2, "pair", register_pair);

fn sorted(data: &mut [Value]) {
    data.sort_unstable();
}

fn sorted_set(data: &mut Vec<Value>) {
    data.sort_unstable();
    data.dedup();
}

// Pairs by key; of pairs with one key, the last stays.
fn sorted_map(data: &mut Vec<Value>) {
    let mut pairs: Vec<[Value; 2]> = data.chunks(2).map(|kv| [kv[0], kv[1]]).collect();
    pairs.reverse();
    pairs.sort_by_key(|kv| kv[0]);
    pairs.dedup_by_key(|kv| kv[0]);
    *data = pairs.concat();
}

pub type VecContainer = Container<VecKind>;
pub type SetContainer = Container<SetKind>;
pub type MultiSetContainer = Container<MultiSetKind>;
pub type MapContainer = Container<MapKind>;
pub type PairContainer = Container<PairKind>;

/// The sort of the containers of one kind over given element sorts.
#[derive(Clone, Debug)]
pub struct Collection<K> {
    name: String,
    elements: Vec<ArcSort>,
    kind: PhantomData<K>,
}

pub type VecSort = Collection<VecKind>;
pub type SetSort = Collection<SetKind>;
pub type MultiSetSort = Collection<MultiSetKind>;
pub type MapSort = Collection<MapKind>;
pub type PairSort = Collection<PairKind>;

fn holds_ids(sort: &ArcSort) -> bool {
    sort.is_eq_sort() || sort.is_eq_container_sort()
}

impl<K: ContainerKind> Collection<K> {
    /// The element sort; of a map its keys, of a pair its first.
    pub fn element(&self) -> ArcSort {
        self.elements[0].clone()
    }

    pub fn key(&self) -> ArcSort {
        self.elements[0].clone()
    }

    /// Of a map its values, of a pair its second.
    pub fn value(&self) -> ArcSort {
        self.elements[K::ARITY - 1].clone()
    }

    pub fn first(&self) -> ArcSort {
        self.key()
    }

    pub fn second(&self) -> ArcSort {
        self.value()
    }

    fn ids(&self) -> [bool; 2] {
        [holds_ids(&self.key()), holds_ids(&self.value())]
    }
}

impl<K: ContainerKind> Sort for Collection<K> {
    fn name(&self) -> &str {
        &self.name
    }

    fn inner_sorts(&self) -> Vec<ArcSort> {
        self.elements.clone()
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }

    fn is_container_sort(&self) -> bool {
        true
    }

    fn is_eq_container_sort(&self) -> bool {
        self.elements.iter().any(holds_ids)
    }

    fn serialized_name(&self, _: &ContainerValues, _: Value) -> String {
        K::HEAD.to_owned()
    }

    fn inner_values(&self, containers: &ContainerValues, value: Value) -> Vec<(ArcSort, Value)> {
        let Some(container) = containers.get_val::<Container<K>>(value) else {
            return Vec::new();
        };
        let sorts = self.elements.iter().cycle();
        sorts.cloned().zip(container.data.iter().copied()).collect()
    }

    fn value_type(&self) -> Option<TypeId> {
        Some(TypeId::of::<Container<K>>())
    }

    fn register_primitives(self: Arc<Self>, eg: &mut EGraph) {
        K::register(&self, eg.database_mut());
    }

    // Terms in canonical order: a set or multiset sorts its elements and a
    // map its keys by term.
    fn reconstruct_termdag_container(
        &self,
        _: &ContainerValues,
        _: Value,
        termdag: &mut TermDag,
        mut terms: Vec<TermId>,
    ) -> TermId {
        if K::HEAD != "vec-of" && K::HEAD != "pair" {
            terms = {
                let mut groups: Vec<&[TermId]> = terms.chunks(K::ARITY).collect();
                groups.sort_by(|a, b| termdag.ast_cmp(a[0], b[0]));
                if K::HEAD == "set-of" {
                    groups.dedup_by(|a, b| a[0] == b[0]);
                }
                groups.concat()
            };
        }
        let head = if terms.is_empty() && K::HEAD == "vec-of" {
            "vec-empty"
        } else {
            K::HEAD
        };
        termdag.app(head.to_owned(), terms)
    }
}

// The sorts that the sort arguments `args` name.
fn sort_args(
    db: &Database,
    args: &[Expr],
    count: usize,
    span: Span,
) -> Result<Vec<ArcSort>, Error> {
    let bad = || Error::Type(span, format!("a sort constructor takes {count} sorts"));
    if args.len() != count {
        return Err(bad());
    }
    args.iter()
        .map(|arg| match arg {
            Expr::Var(_, name) => Ok(db.schema().sort(db.sort(name)?).arcsort().clone()),
            _ => Err(bad()),
        })
        .collect()
}

impl<K: ContainerKind> Presort for Collection<K> {
    fn presort_name() -> &'static str {
        K::PRESORT
    }

    fn reserved_primitives() -> Vec<&'static str> {
        Vec::new()
    }

    fn make_sort(db: &Database, name: String, args: &[Expr], span: Span) -> Result<ArcSort, Error> {
        let elements = sort_args(db, args, K::ARITY, span)?;
        Ok(Arc::new(Self {
            name,
            elements,
            kind: PhantomData,
        }))
    }
}

// The sorts a container's primitives use.
struct Sorts {
    c: SortId,
    k: SortId,
    v: SortId,
    i: SortId,
    u: SortId,
    ids: [bool; 2],
}

impl Sorts {
    fn of<K: ContainerKind>(sort: &Collection<K>, db: &Database) -> Self {
        let id = |sort: &ArcSort| db.sort(sort.name()).expect("a declared sort");
        Self {
            c: db.sort(&sort.name).expect("a declared sort"),
            k: id(&sort.key()),
            v: id(&sort.value()),
            i: db.sort("i64").expect("a base sort"),
            u: db.sort("Unit").expect("a base sort"),
            ids: sort.ids(),
        }
    }
}

fn get<K: ContainerKind>(ctx: &PrimCtx<'_>, value: Value) -> Option<Container<K>> {
    ctx.container_value::<Container<K>>(value).cloned()
}

fn put<K: ContainerKind>(ctx: &mut PrimCtx<'_>, data: Vec<Value>, ids: [bool; 2]) -> Option<Value> {
    Some(ctx.container(Container::<K>::new(data, ids)))
}

// One overload per arity up to `VARIADIC`; `inputs` gives the sorts of one.
fn variadic(
    db: &mut Database,
    name: &str,
    inputs: impl Fn(usize) -> Vec<SortId>,
    output: SortId,
    f: impl Fn(&mut PrimCtx<'_>, &[Value]) -> Option<Value> + Clone + 'static,
) {
    for arity in 0..=VARIADIC {
        let signature = Signature {
            inputs: inputs(arity),
            output,
        };
        db.add_primitive(name, signature, f.clone());
    }
}

fn position(data: &[Value], x: Value) -> Option<usize> {
    data.iter().position(|&y| y == x)
}

#[rustfmt::skip]
fn register_vec(sort: &Collection<VecKind>, db: &mut Database) {
    let Sorts { c, k: e, i, u, ids, .. } = Sorts::of(sort, db);
    variadic(db, "vec-of", |n| vec![e; n], c, move |ctx, xs| put::<VecKind>(ctx, xs.to_vec(), ids));
    variadic(db, "vec-append", |n| vec![c; n], c, move |ctx, xs| {
        let parts: Option<Vec<_>> = xs.iter().map(|&x| get::<VecKind>(ctx, x).map(|v| v.data)).collect();
        put::<VecKind>(ctx, parts?.concat(), ids)
    });
    prim!(db, "vec-empty", [] -> c, |ctx;| put::<VecKind>(ctx, Vec::new(), ids));
    prim!(db, "vec-push", [c, e] -> c, |ctx; xs: Value, x: Value| {
        let mut data = get::<VecKind>(ctx, xs)?.data;
        data.push(x);
        put::<VecKind>(ctx, data, ids)
    });
    prim!(db, "vec-pop", [c] -> c, |ctx; xs: Value| {
        let mut data = get::<VecKind>(ctx, xs)?.data;
        data.pop();
        put::<VecKind>(ctx, data, ids)
    });
    prim!(db, "vec-length", [c] -> i, |ctx; xs: Value| i64::try_from(get::<VecKind>(ctx, xs)?.data.len()).ok());
    prim!(db, "vec-contains", [c, e] -> u, |ctx; xs: Value, x: Value| get::<VecKind>(ctx, xs)?.data.contains(&x).then_some(()));
    prim!(db, "vec-not-contains", [c, e] -> u, |ctx; xs: Value, x: Value| (!get::<VecKind>(ctx, xs)?.data.contains(&x)).then_some(()));
    prim!(db, "vec-get", [c, i] -> e, |ctx; xs: Value, at: i64| get::<VecKind>(ctx, xs)?.data.get(usize::try_from(at).ok()?).copied());
    prim!(db, "vec-set", [c, i, e] -> c, |ctx; xs: Value, at: i64, x: Value| {
        let mut data = get::<VecKind>(ctx, xs)?.data;
        *data.get_mut(usize::try_from(at).ok()?)? = x;
        put::<VecKind>(ctx, data, ids)
    });
    prim!(db, "vec-remove", [c, i] -> c, |ctx; xs: Value, at: i64| {
        let mut data = get::<VecKind>(ctx, xs)?.data;
        let at = usize::try_from(at).ok().filter(|&at| at < data.len())?;
        data.remove(at);
        put::<VecKind>(ctx, data, ids)
    });
    if sort.element().is_eq_sort() {
        let write = crate::WriteState::valid_contexts();
        prim!(db, "vec-union", [c, c] -> c, in write, |ctx; xs: Value, ys: Value| {
            let (left, right) = (get::<VecKind>(ctx, xs)?.data, get::<VecKind>(ctx, ys)?.data);
            if left.len() != right.len() {
                return None;
            }
            left.into_iter().zip(right).for_each(|(l, r)| ctx.union(l, r));
            Some(xs)
        });
    }
    if sort.element().name() == "i64" {
        prim!(db, "vec-range", [i] -> c, |ctx; end: i64| {
            let data = (0..end.max(0)).map(|x| ctx.base(x)).collect();
            put::<VecKind>(ctx, data, ids)
        });
    }
}

#[rustfmt::skip]
fn register_set(sort: &Collection<SetKind>, db: &mut Database) {
    let Sorts { c, k: e, i, u, ids, .. } = Sorts::of(sort, db);
    variadic(db, "set-of", |n| vec![e; n], c, move |ctx, xs| put::<SetKind>(ctx, xs.to_vec(), ids));
    prim!(db, "set-empty", [] -> c, |ctx;| put::<SetKind>(ctx, Vec::new(), ids));
    prim!(db, "set-get", [c, i] -> e, |ctx; xs: Value, at: i64| get::<SetKind>(ctx, xs)?.data.get(usize::try_from(at).ok()?).copied());
    prim!(db, "set-insert", [c, e] -> c, |ctx; xs: Value, x: Value| {
        let data = [get::<SetKind>(ctx, xs)?.data, vec![x]].concat();
        put::<SetKind>(ctx, data, ids)
    });
    prim!(db, "set-remove", [c, e] -> c, |ctx; xs: Value, x: Value| {
        let mut data = get::<SetKind>(ctx, xs)?.data;
        data.retain(|&y| y != x);
        put::<SetKind>(ctx, data, ids)
    });
    prim!(db, "set-length", [c] -> i, |ctx; xs: Value| i64::try_from(get::<SetKind>(ctx, xs)?.data.len()).ok());
    prim!(db, "set-contains", [c, e] -> u, |ctx; xs: Value, x: Value| get::<SetKind>(ctx, xs)?.data.binary_search(&x).ok().map(|_| ()));
    prim!(db, "set-not-contains", [c, e] -> u, |ctx; xs: Value, x: Value| get::<SetKind>(ctx, xs)?.data.binary_search(&x).err().map(|_| ()));
    prim!(db, "set-union", [c, c] -> c, |ctx; xs: Value, ys: Value| {
        let data = [get::<SetKind>(ctx, xs)?.data, get::<SetKind>(ctx, ys)?.data].concat();
        put::<SetKind>(ctx, data, ids)
    });
    for (name, keep) in [("set-diff", false), ("set-intersect", true)] {
        prim!(db, name, [c, c] -> c, |ctx; xs: Value, ys: Value| {
            let (mut data, other) = (get::<SetKind>(ctx, xs)?.data, get::<SetKind>(ctx, ys)?.data);
            data.retain(|x| other.binary_search(x).is_ok() == keep);
            put::<SetKind>(ctx, data, ids)
        });
    }
}

// The key-value pairs of a map container.
fn pairs(map: &MapContainer) -> impl Iterator<Item = (Value, Value)> + '_ {
    map.data.chunks(2).map(|kv| (kv[0], kv[1]))
}

#[rustfmt::skip]
fn register_map(sort: &Collection<MapKind>, db: &mut Database) {
    let Sorts { c, k, v, i, u, ids } = Sorts::of(sort, db);
    for n in 0..=VARIADIC / 2 {
        let signature = Signature { inputs: [k, v].repeat(n), output: c };
        db.add_primitive("map-of", signature, move |ctx, kvs| put::<MapKind>(ctx, kvs.to_vec(), ids));
    }
    prim!(db, "map-empty", [] -> c, |ctx;| put::<MapKind>(ctx, Vec::new(), ids));
    prim!(db, "map-get", [c, k] -> v, |ctx; xs: Value, key: Value| pairs(&get::<MapKind>(ctx, xs)?).find(|kv| kv.0 == key).map(|kv| kv.1));
    prim!(db, "map-insert", [c, k, v] -> c, |ctx; xs: Value, key: Value, value: Value| {
        let data = [get::<MapKind>(ctx, xs)?.data, vec![key, value]].concat();
        put::<MapKind>(ctx, data, ids)
    });
    prim!(db, "map-remove", [c, k] -> c, |ctx; xs: Value, key: Value| {
        let data = pairs(&get::<MapKind>(ctx, xs)?).filter(|kv| kv.0 != key).flat_map(|kv| [kv.0, kv.1]).collect();
        put::<MapKind>(ctx, data, ids)
    });
    prim!(db, "map-length", [c] -> i, |ctx; xs: Value| i64::try_from(get::<MapKind>(ctx, xs)?.data.len() / 2).ok());
    prim!(db, "map-contains", [c, k] -> u, |ctx; xs: Value, key: Value| pairs(&get::<MapKind>(ctx, xs)?).any(|kv| kv.0 == key).then_some(()));
    prim!(db, "map-not-contains", [c, k] -> u, |ctx; xs: Value, key: Value| (!pairs(&get::<MapKind>(ctx, xs)?).any(|kv| kv.0 == key)).then_some(()));
}

#[rustfmt::skip]
fn register_pair(sort: &Collection<PairKind>, db: &mut Database) {
    let Sorts { c, k, v, ids, .. } = Sorts::of(sort, db);
    prim!(db, "pair", [k, v] -> c, |ctx; x: Value, y: Value| put::<PairKind>(ctx, vec![x, y], ids));
    prim!(db, "pair-first", [c] -> k, |ctx; xs: Value| Some(get::<PairKind>(ctx, xs)?.data[0]));
    prim!(db, "pair-second", [c] -> v, |ctx; xs: Value| Some(get::<PairKind>(ctx, xs)?.data[1]));
}

// Each distinct element of a sorted multiset with its count.
fn counts(data: &[Value]) -> Vec<(Value, usize)> {
    data.chunk_by(|a, b| a == b)
        .map(|run| (run[0], run.len()))
        .collect()
}

// `data` with one of each of `other` taken out, if it holds them all.
fn subtract(mut data: Vec<Value>, other: &[Value]) -> Option<Vec<Value>> {
    for &x in other {
        data.remove(position(&data, x)?);
    }
    Some(data)
}

#[rustfmt::skip]
fn register_multiset(sort: &Collection<MultiSetKind>, db: &mut Database) {
    type M = MultiSetKind;
    let Sorts { c, k: e, i, u, ids, .. } = Sorts::of(sort, db);
    variadic(db, "multiset-of", |n| vec![e; n], c, move |ctx, xs| put::<M>(ctx, xs.to_vec(), ids));
    prim!(db, "multiset-single", [e, i] -> c, |ctx; x: Value, n: i64| put::<M>(ctx, vec![x; usize::try_from(n).ok()?], ids));
    prim!(db, "multiset-pick", [c] -> e, |ctx; xs: Value| get::<M>(ctx, xs)?.data.first().copied());
    prim!(db, "multiset-insert", [c, e] -> c, |ctx; xs: Value, x: Value| put::<M>(ctx, [get::<M>(ctx, xs)?.data, vec![x]].concat(), ids));
    for (name, swapped) in [("multiset-remove", false), ("multiset-remove-swapped", true)] {
        let inputs = if swapped { vec![e, c] } else { vec![c, e] };
        db.add_primitive(name, Signature { inputs, output: c }, move |ctx, args| {
            let (xs, x) = if swapped { (args[1], args[0]) } else { (args[0], args[1]) };
            put::<M>(ctx, subtract(get::<M>(ctx, xs)?.data, &[x])?, ids)
        });
    }
    for (name, swapped) in [("multiset-subtract", false), ("multiset-subtract-swapped", true)] {
        prim!(db, name, [c, c] -> c, |ctx; a: Value, b: Value| {
            let (xs, other) = if swapped { (b, a) } else { (a, b) };
            put::<M>(ctx, subtract(get::<M>(ctx, xs)?.data, &get::<M>(ctx, other)?.data)?, ids)
        });
    }
    prim!(db, "multiset-length", [c] -> i, |ctx; xs: Value| i64::try_from(get::<M>(ctx, xs)?.data.len()).ok());
    for (name, swapped, want) in [
        ("multiset-contains", false, true),
        ("multiset-not-contains", false, false),
        ("multiset-contains-swapped", true, true),
        ("multiset-not-contains-swapped", true, false),
    ] {
        let inputs = if swapped { vec![e, c] } else { vec![c, e] };
        db.add_primitive(name, Signature { inputs, output: u }, move |ctx, args| {
            let (xs, x) = if swapped { (args[1], args[0]) } else { (args[0], args[1]) };
            (get::<M>(ctx, xs)?.data.contains(&x) == want).then(|| ctx.base(()))
        });
    }
    prim!(db, "multiset-intersection", [c, c] -> c, |ctx; xs: Value, ys: Value| {
        let mut other = get::<M>(ctx, ys)?.data;
        let data = get::<M>(ctx, xs)?.data.into_iter().filter(|&x| position(&other, x).map(|at| other.remove(at)).is_some()).collect();
        put::<M>(ctx, data, ids)
    });
    prim!(db, "multiset-sum", [c, c] -> c, |ctx; xs: Value, ys: Value| put::<M>(ctx, [get::<M>(ctx, xs)?.data, get::<M>(ctx, ys)?.data].concat(), ids));
    prim!(db, "multiset-reset-counts", [c] -> c, |ctx; xs: Value| {
        let mut data = get::<M>(ctx, xs)?.data;
        data.dedup();
        put::<M>(ctx, data, ids)
    });
    prim!(db, "multiset-pick-max", [c] -> e, |ctx; xs: Value| counts(&get::<M>(ctx, xs)?.data).into_iter().max_by_key(|xc| xc.1).map(|xc| xc.0));
    prim!(db, "multiset-count", [c, e] -> i, |ctx; xs: Value, x: Value| i64::try_from(get::<M>(ctx, xs)?.data.iter().filter(|&&y| y == x).count()).ok());
    if sort.element().is_eq_sort() {
        let write = crate::WriteState::valid_contexts();
        prim!(db, "multiset-union-values", [c] -> e, in write, |ctx; xs: Value| {
            let data = get::<M>(ctx, xs)?.data;
            let first = *data.first()?;
            data.into_iter().for_each(|x| ctx.union(first, x));
            Some(first)
        });
    }
}

/// What an `UnstableFn` value calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Target {
    Table(crate::TableId),
    Prim(crate::ExternId),
}

/// A function with its first arguments given: a table or a primitive.
#[derive(Clone, Debug)]
pub struct FunctionContainer {
    target: Target,
    name: String,
    args: Vec<(ArcSort, Value)>,
}

impl PartialEq for FunctionContainer {
    fn eq(&self, other: &Self) -> bool {
        let values = |f: &Self| f.args.iter().map(|(_, value)| *value).collect::<Vec<_>>();
        self.target == other.target && values(self) == values(other)
    }
}

impl Eq for FunctionContainer {}

impl Hash for FunctionContainer {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.target.hash(state);
        self.args.iter().for_each(|(_, value)| value.hash(state));
    }
}

impl ContainerValue for FunctionContainer {
    fn rebuild_contents(&mut self, find: &dyn Fn(Value) -> Value) -> bool {
        let mut changed = false;
        for (sort, value) in &mut self.args {
            if holds_ids(sort) {
                let found = find(*value);
                changed |= found != *value;
                *value = found;
            }
        }
        changed
    }

    fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        self.args.iter().map(|(_, value)| *value)
    }
}

impl FunctionContainer {
    /// Calls the function on its given arguments, then `args`.
    pub fn apply(&self, ctx: &mut PrimCtx<'_>, args: &[Value]) -> Option<Value> {
        let given = self.args.iter().map(|(_, value)| *value);
        let args: Vec<Value> = given.chain(args.iter().copied()).collect();
        match self.target {
            Target::Table(table) => ctx.entry(table, &args),
            Target::Prim(prim) => ctx.call(prim, &args),
        }
    }
}

/// The sort `(UnstableFn (inputs...) output)` of functions.
#[derive(Clone, Debug)]
pub struct FunctionSort {
    name: String,
    inputs: Vec<ArcSort>,
    output: ArcSort,
}

impl FunctionSort {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn inputs(&self) -> &[ArcSort] {
        &self.inputs
    }

    pub fn output(&self) -> ArcSort {
        self.output.clone()
    }
}

impl Presort for FunctionSort {
    fn presort_name() -> &'static str {
        "UnstableFn"
    }

    fn reserved_primitives() -> Vec<&'static str> {
        vec!["unstable-fn", "unstable-app"]
    }

    fn make_sort(db: &Database, name: String, args: &[Expr], span: Span) -> Result<ArcSort, Error> {
        let [inputs, output] = args else {
            return Err(Error::Type(
                span,
                "UnstableFn takes inputs and an output".to_owned(),
            ));
        };
        let inputs = match inputs {
            Expr::Call(span, first, rest) => {
                let mut all = vec![Expr::Var(*span, first.clone())];
                all.extend(rest.iter().cloned());
                sort_args(db, &all, all.len(), *span)?
            }
            Expr::Lit(_, Literal::Unit) => Vec::new(),
            _ => {
                return Err(Error::Type(
                    span,
                    "UnstableFn inputs are a list of sorts".to_owned(),
                ));
            }
        };
        let output = sort_args(db, std::slice::from_ref(output), 1, span)?.remove(0);
        Ok(Arc::new(Self {
            name,
            inputs,
            output,
        }))
    }
}

impl Sort for FunctionSort {
    fn name(&self) -> &str {
        &self.name
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }

    fn is_container_sort(&self) -> bool {
        true
    }

    fn is_eq_container_sort(&self) -> bool {
        self.inputs.iter().any(holds_ids)
    }

    fn serialized_name(&self, containers: &ContainerValues, value: Value) -> String {
        let f = containers.get_val::<FunctionContainer>(value);
        f.map_or_else(|| self.name.clone(), |f| f.name.clone())
    }

    fn inner_values(&self, containers: &ContainerValues, value: Value) -> Vec<(ArcSort, Value)> {
        let f = containers.get_val::<FunctionContainer>(value);
        f.map_or_else(Vec::new, |f| f.args.clone())
    }

    fn value_type(&self) -> Option<TypeId> {
        Some(TypeId::of::<FunctionContainer>())
    }

    fn reconstruct_termdag_container(
        &self,
        containers: &ContainerValues,
        value: Value,
        termdag: &mut TermDag,
        terms: Vec<TermId>,
    ) -> TermId {
        let name = self.serialized_name(containers, value);
        let head = termdag.lit(Literal::String(name));
        termdag.app("unstable-fn".to_owned(), [vec![head], terms].concat())
    }
}

fn sort_id(db: &Database, sort: &ArcSort) -> SortId {
    db.sort(sort.name()).expect("a declared sort")
}

fn downcast<S: Sort>(sort: &ArcSort) -> Option<Arc<S>> {
    sort.clone().as_arc_any().downcast::<S>().ok()
}

// Registers what the declared `UnstableFn` sorts make possible: making and
// applying their functions over every table and primitive that fits, and
// the higher-order primitives of vectors and multisets.
pub(crate) fn register_functions(db: &mut Database) {
    let sorts: Vec<ArcSort> = db
        .schema()
        .sorts()
        .map(|(_, sort)| sort.arcsort().clone())
        .collect();
    let functions: Vec<Arc<FunctionSort>> = sorts.iter().filter_map(downcast).collect();
    for f in &functions {
        register_function(db, f);
        for sort in &sorts {
            if let Some(input) = downcast::<VecSort>(sort) {
                for output in sorts.iter().filter_map(downcast::<VecSort>) {
                    register_mapping(db, "unstable-vec-map", f, &input, &output);
                }
            }
            if let Some(ms) = downcast::<MultiSetSort>(sort) {
                for output in sorts.iter().filter_map(downcast::<MultiSetSort>) {
                    register_mapping(db, "unstable-multiset-map", f, &ms, &output);
                }
                register_multiset_function(db, f, &ms);
            }
        }
    }
    for outer in sorts.iter().filter_map(downcast::<MultiSetSort>) {
        if let Some(inner) = downcast::<MultiSetSort>(&outer.element()) {
            register_sum_multisets(db, &outer, &inner);
        }
    }
}

fn same(a: &[ArcSort], b: &[ArcSort]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.name() == b.name())
}

// `(unstable-fn "name" given...)` names its target by a literal, which
// rules see as a call of `@fn name`: one overload per table or primitive
// `name` that the given arguments and `f` complete.
pub(crate) const FN_PREFIX: &str = "@fn ";

fn register_function(db: &mut Database, f: &Arc<FunctionSort>) {
    let fs = sort_id(db, &(f.clone() as ArcSort));
    let output = sort_id(db, &f.output);
    let inputs: Vec<SortId> = f.inputs.iter().map(|s| sort_id(db, s)).collect();
    let schema = db.schema();
    let tables = (schema.tables())
        .filter(|(_, t)| t.kind() != TableKind::Relation && !t.name().starts_with('@'))
        .map(|(id, t)| {
            let (keys, out) = t.columns().split_at(t.key_arity());
            (
                Target::Table(id),
                t.name().to_owned(),
                keys.to_vec(),
                out[0],
            )
        });
    let prims = (0..db.prims.len())
        .map(crate::ExternId::new)
        .filter_map(|p| {
            let (name, signature) = schema.primitive(p);
            let named = !name.starts_with('@') && !name.starts_with("unstable-");
            named.then(|| {
                (
                    Target::Prim(p),
                    name.to_owned(),
                    signature.inputs.clone(),
                    signature.output,
                )
            })
        });
    let targets: Vec<_> = tables
        .chain(prims)
        .filter(|(.., keys, out)| *out == output && keys.ends_with(&inputs))
        .collect();
    for (target, name, keys, _) in targets {
        let partial = &keys[..keys.len() - inputs.len()];
        let sorts: Vec<ArcSort> = partial
            .iter()
            .map(|&s| db.schema().sort(s).arcsort().clone())
            .collect();
        let signature = Signature {
            inputs: partial.to_vec(),
            output: fs,
        };
        let head = format!("{FN_PREFIX}{name}");
        db.add_primitive(&head, signature, move |ctx, args| {
            let args = sorts.iter().cloned().zip(args.iter().copied()).collect();
            let name = name.clone();
            Some(ctx.container(FunctionContainer { target, name, args }))
        });
    }
    let signature = Signature {
        inputs: [vec![fs], inputs].concat(),
        output,
    };
    db.add_primitive("unstable-app", signature, |ctx, args| {
        let f = ctx.container_value::<FunctionContainer>(args[0])?.clone();
        f.apply(ctx, &args[1..])
    });
}

fn function(ctx: &PrimCtx<'_>, value: Value) -> Option<FunctionContainer> {
    ctx.container_value::<FunctionContainer>(value).cloned()
}

// `(unstable-vec-map f xs)` and `(unstable-multiset-map f xs)`: the results
// of `f` on each element, skipping those it has none for.
fn register_mapping<K: ContainerKind>(
    db: &mut Database,
    name: &str,
    f: &Arc<FunctionSort>,
    input: &Collection<K>,
    output: &Collection<K>,
) {
    if !same(&f.inputs, &[input.element()]) || f.output.name() != output.element().name() {
        return;
    }
    let (fs, i, o, ids) = (
        sort_id(db, &(f.clone() as ArcSort)),
        Sorts::of(input, db).c,
        Sorts::of(output, db).c,
        output.ids(),
    );
    prim!(db, name, [fs, i] -> o, |ctx; f: Value, xs: Value| {
        let (f, xs) = (function(ctx, f)?, get::<K>(ctx, xs)?);
        let data = xs.data.iter().filter_map(|&x| f.apply(ctx, &[x])).collect();
        put::<K>(ctx, data, ids)
    });
}

#[rustfmt::skip]
fn register_multiset_function(db: &mut Database, f: &Arc<FunctionSort>, ms: &MultiSetSort) {
    type M = MultiSetKind;
    let (fs, Sorts { c, k: e, u, ids, .. }) = (sort_id(db, &(f.clone() as ArcSort)), Sorts::of(ms, db));
    let (element, output) = (ms.element(), f.output.name());
    if same(&f.inputs, std::slice::from_ref(&element)) && output == "Unit" {
        for (name, keep) in [("unstable-multiset-filter", true), ("unstable-multiset-filter-not", false)] {
            prim!(db, name, [fs, c] -> c, |ctx; f: Value, xs: Value| {
                let (f, xs) = (function(ctx, f)?, get::<M>(ctx, xs)?);
                let data = xs.data.into_iter().filter(|&x| f.apply(ctx, &[x]).is_some() == keep).collect();
                put::<M>(ctx, data, ids)
            });
        }
    }
    if same(&f.inputs, &[element.clone(), element.clone()]) && output == element.name() {
        prim!(db, "unstable-multiset-reduce", [fs, e, c] -> e, |ctx; f: Value, init: Value, xs: Value| {
            let (f, xs) = (function(ctx, f)?, get::<M>(ctx, xs)?.data);
            let Some((&first, rest)) = xs.split_first() else { return Some(init) };
            rest.iter().try_fold(first, |acc, &x| f.apply(ctx, &[acc, x]))
        });
    }
    if same(&f.inputs, std::slice::from_ref(&element)) && output == ms.name() {
        prim!(db, "unstable-multiset-flat-map", [fs, c] -> c, |ctx; f: Value, xs: Value| {
            let (f, xs) = (function(ctx, f)?, get::<M>(ctx, xs)?);
            let mut data = Vec::new();
            for x in xs.data {
                match f.apply(ctx, &[x]) {
                    Some(mapped) => data.extend(get::<M>(ctx, mapped)?.data),
                    None => data.push(x),
                }
            }
            put::<M>(ctx, data, ids)
        });
    }
    if f.inputs.len() == 2 && f.inputs[0].name() == ms.name() && f.inputs[1].name() == element.name() && output == "i64" {
        // A count per distinct element in the function's table, stopping at
        // the first element it already has one for.
        let full = crate::FullState::valid_contexts();
        prim!(db, "unstable-multiset-fill-index", [c, fs] -> u, in full, |ctx; xs: Value, f: Value| {
            let Target::Table(table) = function(ctx, f)?.target else { return None };
            for (x, count) in counts(&get::<M>(ctx, xs)?.data) {
                if ctx.lookup(table, &[xs, x]).is_some() {
                    break;
                }
                let count = ctx.base(i64::try_from(count).ok()?);
                ctx.insert(table, &[xs, x, count]);
            }
            Some(())
        });
        let write = crate::WriteState::valid_contexts();
        prim!(db, "unstable-multiset-clear-index", [c, fs] -> u, in write, |ctx; xs: Value, f: Value| {
            let Target::Table(table) = function(ctx, f)?.target else { return None };
            counts(&get::<M>(ctx, xs)?.data).into_iter().for_each(|(x, _)| ctx.remove(table, &[xs, x]));
            Some(())
        });
    }
}

fn register_sum_multisets(db: &mut Database, outer: &MultiSetSort, inner: &MultiSetSort) {
    let (o, Sorts { c, ids, .. }) = (Sorts::of(outer, db).c, Sorts::of(inner, db));
    prim!(db, "multiset-sum-multisets", [o] -> c, |ctx; xs: Value| {
        let outer = get::<MultiSetKind>(ctx, xs)?.data;
        let inner: Option<Vec<Vec<Value>>> = outer.iter().map(|&x| get::<MultiSetKind>(ctx, x).map(|m| m.data)).collect();
        put::<MultiSetKind>(ctx, inner?.concat(), ids)
    });
}

// Interns a container of one sort from its elements.
pub(crate) type Make = Box<dyn Fn(&mut Database, Vec<Value>) -> Option<Value>>;

// How to make the containers of `sort`, if it is a sort of the generic
// container.
pub(crate) fn rebuilt(sort: &ArcSort) -> Option<Make> {
    fn maker<K: ContainerKind>(sort: &ArcSort) -> Option<Make> {
        let ids = downcast::<Collection<K>>(sort)?.ids();
        Some(Box::new(move |db: &mut Database, data| {
            Some(db.container(Container::<K>::new(data, ids)))
        }))
    }
    (maker::<VecKind>(sort))
        .or_else(|| maker::<SetKind>(sort))
        .or_else(|| maker::<MultiSetKind>(sort))
        .or_else(|| maker::<MapKind>(sort))
        .or_else(|| maker::<PairKind>(sort))
}
