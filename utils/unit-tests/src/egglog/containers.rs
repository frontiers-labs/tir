//! Container sorts through the Rust API: the primitives of egglog's
//! src/sort/{vec,set,map,multiset,pair,fn}.rs, and containers made equal by
//! unions (egglog's tests/container_rebuild.rs).

use tir_egglog::ast::{Command, Fact};
use tir_egglog::prelude::*;
use tir_egglog::{Error, Value};

fn call(head: &str, args: Vec<Expr>) -> Expr {
    exprs::call(head, args)
}

fn ints(xs: &[i64]) -> Vec<Expr> {
    xs.iter().map(|&x| exprs::int(x)).collect()
}

// An e-graph with the container sorts `(sort name (presort args...))`.
fn with_sorts(sorts: &[(&str, &str, &[&str])]) -> EGraph {
    let mut eg = EGraph::default();
    let commands = sorts.iter().map(|(name, presort, args)| Command::Sort {
        span: span!(),
        name: (*name).to_owned(),
        presort_and_args: Some((
            (*presort).to_owned(),
            args.iter().map(|&a| exprs::var(a)).collect(),
        )),
    });
    eg.run_program(commands.collect()).unwrap();
    eg
}

fn value(eg: &mut EGraph, expr: Expr) -> Value {
    eg.eval_expr(&expr).unwrap().1
}

fn int(eg: &mut EGraph, expr: Expr) -> i64 {
    let v = value(eg, expr);
    eg.value_to_base(v)
}

// Whether `expr` has a value: a primitive with no result has none.
fn defined(eg: &mut EGraph, expr: Expr) -> bool {
    eg.eval_expr(&expr).is_ok()
}

fn equal(eg: &mut EGraph, a: Expr, b: Expr) -> bool {
    value(eg, a) == value(eg, b)
}

#[test]
fn vec_primitives() {
    let mut eg = with_sorts(&[("IVec", "Vec", &["i64"])]);
    let v123 = call("vec-of", ints(&[1, 2, 3]));
    assert_eq!(int(&mut eg, call("vec-length", vec![v123.clone()])), 3);
    assert_eq!(
        int(&mut eg, call("vec-get", vec![v123.clone(), exprs::int(1)])),
        2
    );
    assert!(!defined(
        &mut eg,
        call("vec-get", vec![v123.clone(), exprs::int(3)])
    ));
    let pushed = call(
        "vec-push",
        vec![call("vec-of", ints(&[1, 2])), exprs::int(3)],
    );
    assert!(equal(&mut eg, pushed, v123.clone()));
    assert!(equal(
        &mut eg,
        call("vec-pop", vec![v123.clone()]),
        call("vec-of", ints(&[1, 2]))
    ));
    let set = call("vec-set", vec![v123.clone(), exprs::int(0), exprs::int(9)]);
    assert!(equal(&mut eg, set, call("vec-of", ints(&[9, 2, 3]))));
    let removed = call("vec-remove", vec![v123.clone(), exprs::int(1)]);
    assert!(equal(&mut eg, removed, call("vec-of", ints(&[1, 3]))));
    assert!(!defined(
        &mut eg,
        call("vec-remove", vec![v123.clone(), exprs::int(5)])
    ));
    assert!(defined(
        &mut eg,
        call("vec-contains", vec![v123.clone(), exprs::int(2)])
    ));
    assert!(!defined(
        &mut eg,
        call("vec-contains", vec![v123.clone(), exprs::int(4)])
    ));
    assert!(defined(
        &mut eg,
        call("vec-not-contains", vec![v123.clone(), exprs::int(4)])
    ));
    let appended = call(
        "vec-append",
        vec![call("vec-of", ints(&[1])), call("vec-of", ints(&[2, 3]))],
    );
    assert!(equal(&mut eg, appended, v123.clone()));
    assert!(equal(
        &mut eg,
        call("vec-range", ints(&[3])),
        call("vec-of", ints(&[0, 1, 2]))
    ));
    assert!(equal(
        &mut eg,
        call("vec-empty", vec![]),
        call("vec-of", vec![])
    ));
    let (termdag, term, _) = {
        let (sort, v) = eg.eval_expr(&v123).unwrap();
        eg.extract_value(&sort, v).unwrap()
    };
    assert_eq!(termdag.to_string(term), "(vec-of 1 2 3)");
}

#[test]
fn set_primitives() {
    let mut eg = with_sorts(&[("ISet", "Set", &["i64"])]);
    let s123 = call("set-of", ints(&[1, 2, 3]));
    assert!(equal(
        &mut eg,
        call("set-of", ints(&[3, 1, 2, 1])),
        s123.clone()
    ));
    assert_eq!(int(&mut eg, call("set-length", vec![s123.clone()])), 3);
    assert_eq!(
        int(&mut eg, call("set-get", vec![s123.clone(), exprs::int(0)])),
        1
    );
    let inserted = call(
        "set-insert",
        vec![call("set-of", ints(&[1, 3])), exprs::int(2)],
    );
    assert!(equal(&mut eg, inserted, s123.clone()));
    let removed = call("set-remove", vec![s123.clone(), exprs::int(2)]);
    assert!(equal(&mut eg, removed, call("set-of", ints(&[1, 3]))));
    assert!(defined(
        &mut eg,
        call("set-contains", vec![s123.clone(), exprs::int(3)])
    ));
    assert!(defined(
        &mut eg,
        call("set-not-contains", vec![s123.clone(), exprs::int(4)])
    ));
    let (a, b) = (call("set-of", ints(&[1, 2])), call("set-of", ints(&[2, 3])));
    assert!(equal(
        &mut eg,
        call("set-union", vec![a.clone(), b.clone()]),
        s123
    ));
    assert!(equal(
        &mut eg,
        call("set-diff", vec![a.clone(), b.clone()]),
        call("set-of", ints(&[1]))
    ));
    assert!(equal(
        &mut eg,
        call("set-intersect", vec![a, b]),
        call("set-of", ints(&[2]))
    ));
}

#[test]
fn map_primitives() {
    let mut eg = with_sorts(&[("IMap", "Map", &["i64", "String"])]);
    let one = call(
        "map-insert",
        vec![
            call("map-empty", vec![]),
            exprs::int(1),
            exprs::string("one"),
        ],
    );
    let both = call(
        "map-insert",
        vec![one.clone(), exprs::int(2), exprs::string("two")],
    );
    let got = value(&mut eg, call("map-get", vec![both.clone(), exprs::int(2)]));
    assert_eq!(eg.value_to_base::<String>(got), "two");
    assert!(!defined(
        &mut eg,
        call("map-get", vec![one.clone(), exprs::int(2)])
    ));
    assert_eq!(int(&mut eg, call("map-length", vec![both.clone()])), 2);
    assert!(defined(
        &mut eg,
        call("map-contains", vec![both.clone(), exprs::int(1)])
    ));
    assert!(defined(
        &mut eg,
        call("map-not-contains", vec![one.clone(), exprs::int(2)])
    ));
    assert!(equal(
        &mut eg,
        call("map-remove", vec![both.clone(), exprs::int(2)]),
        one.clone()
    ));
    let replaced = call("map-insert", vec![one, exprs::int(1), exprs::string("uno")]);
    let got = value(&mut eg, call("map-get", vec![replaced, exprs::int(1)]));
    assert_eq!(
        eg.value_to_base::<String>(got),
        "uno",
        "a second insert of a key replaces it"
    );
    let flat = call(
        "map-of",
        vec![
            exprs::int(2),
            exprs::string("two"),
            exprs::int(1),
            exprs::string("one"),
        ],
    );
    assert!(equal(&mut eg, flat, both));
}

#[test]
fn multiset_primitives() {
    let mut eg = with_sorts(&[("IMS", "MultiSet", &["i64"])]);
    let ms = call("multiset-of", ints(&[1, 2, 2, 3]));
    assert!(equal(
        &mut eg,
        call("multiset-of", ints(&[2, 3, 2, 1])),
        ms.clone()
    ));
    assert!(
        !equal(&mut eg, call("multiset-of", ints(&[1, 2, 3])), ms.clone()),
        "counts matter"
    );
    assert_eq!(int(&mut eg, call("multiset-length", vec![ms.clone()])), 4);
    assert_eq!(
        int(
            &mut eg,
            call("multiset-count", vec![ms.clone(), exprs::int(2)])
        ),
        2
    );
    assert_eq!(int(&mut eg, call("multiset-pick", vec![ms.clone()])), 1);
    assert_eq!(int(&mut eg, call("multiset-pick-max", vec![ms.clone()])), 2);
    let removed = call("multiset-remove", vec![ms.clone(), exprs::int(2)]);
    assert!(equal(
        &mut eg,
        removed,
        call("multiset-of", ints(&[1, 2, 3]))
    ));
    assert!(!defined(
        &mut eg,
        call("multiset-remove", vec![ms.clone(), exprs::int(7)])
    ));
    let swapped = call("multiset-remove-swapped", vec![exprs::int(3), ms.clone()]);
    assert!(equal(
        &mut eg,
        swapped,
        call("multiset-of", ints(&[1, 2, 2]))
    ));
    let inserted = call(
        "multiset-insert",
        vec![call("multiset-of", ints(&[1, 2, 3])), exprs::int(2)],
    );
    assert!(equal(&mut eg, inserted, ms.clone()));
    let (a, b) = (
        call("multiset-of", ints(&[1, 2, 2])),
        call("multiset-of", ints(&[2, 2, 2, 3])),
    );
    let sum = call("multiset-sum", vec![a.clone(), b.clone()]);
    assert!(equal(
        &mut eg,
        sum,
        call("multiset-of", ints(&[1, 2, 2, 2, 2, 2, 3]))
    ));
    let both = call("multiset-intersection", vec![a.clone(), b.clone()]);
    assert!(equal(&mut eg, both, call("multiset-of", ints(&[2, 2]))));
    let taken = call("multiset-subtract", vec![ms.clone(), a.clone()]);
    assert!(equal(&mut eg, taken, call("multiset-of", ints(&[3]))));
    assert!(
        !defined(&mut eg, call("multiset-subtract", vec![a, b])),
        "a has no 3 to take"
    );
    assert!(equal(
        &mut eg,
        call("multiset-reset-counts", vec![ms.clone()]),
        call("multiset-of", ints(&[1, 2, 3]))
    ));
    assert!(equal(
        &mut eg,
        call("multiset-single", ints(&[5, 2])),
        call("multiset-of", ints(&[5, 5]))
    ));
    assert!(defined(
        &mut eg,
        call("multiset-contains", vec![ms.clone(), exprs::int(3)])
    ));
    assert!(defined(
        &mut eg,
        call("multiset-not-contains-swapped", vec![exprs::int(4), ms])
    ));
}

#[test]
fn pair_primitives() {
    let mut eg = with_sorts(&[("IS", "Pair", &["i64", "String"])]);
    let pair = call("pair", vec![exprs::int(4), exprs::string("four")]);
    assert_eq!(int(&mut eg, call("pair-first", vec![pair.clone()])), 4);
    let second = value(&mut eg, call("pair-second", vec![pair]));
    assert_eq!(eg.value_to_base::<String>(second), "four");
}

#[test]
fn function_values_apply_tables_and_primitives() -> Result<(), Error> {
    let mut eg = with_sorts(&[("IVec", "Vec", &["i64"])]);
    datatype!(&mut eg, (datatype Math (Num i64)));
    let inputs = call("i64", vec![]);
    let fn_sort = |output: &str| Command::Sort {
        span: span!(),
        name: format!("To{output}"),
        presort_and_args: Some((
            "UnstableFn".to_owned(),
            vec![inputs.clone(), exprs::var(output)],
        )),
    };
    eg.run_program(vec![fn_sort("i64"), fn_sort("Math")])
        .unwrap();
    let plus_ten = call("unstable-fn", vec![exprs::string("+"), exprs::int(10)]);
    assert_eq!(
        int(
            &mut eg,
            call("unstable-app", vec![plus_ten.clone(), exprs::int(5)])
        ),
        15
    );
    let num = call("unstable-fn", vec![exprs::string("Num")]);
    let applied = call("unstable-app", vec![num, exprs::int(5)]);
    assert!(
        equal(&mut eg, applied, expr!((Num 5))),
        "a constructor makes the e-class"
    );
    let mapped = call(
        "unstable-vec-map",
        vec![plus_ten, call("vec-of", ints(&[1, 2]))],
    );
    assert!(equal(&mut eg, mapped, call("vec-of", ints(&[11, 12]))));
    Ok(())
}

fn check(eg: &mut EGraph, facts: Vec<Fact>) {
    eg.run_program(vec![Command::Check(span!(), facts)])
        .unwrap();
}

// (sort Math) with (A), (B) and (Holds C) over a container sort C.
fn holder(container: (&str, &[&str])) -> EGraph {
    let mut eg = with_sorts(&[]);
    add_sort(&mut eg, "Math").unwrap();
    let make = |eg: &mut EGraph, name: &str, args: &[&str]| {
        let schema = tir_egglog::ast::Schema::new(
            args.iter().map(|&a| a.to_owned()).collect(),
            "Math".to_owned(),
        );
        add_constructor(eg, name, schema, None, false).unwrap();
    };
    make(&mut eg, "A", &[]);
    make(&mut eg, "B", &[]);
    let presort = Some((
        container.0.to_owned(),
        container.1.iter().map(|&a| exprs::var(a)).collect(),
    ));
    eg.run_program(vec![Command::Sort {
        span: span!(),
        name: "C".into(),
        presort_and_args: presort,
    }])
    .unwrap();
    make(&mut eg, "Holds", &["C"]);
    eg
}

// Holds of two containers made equal by `(union (A) (B))`: their rows
// become one, as egglog's container_rebuild tests check.
fn congruent_after_union(container: (&str, &[&str]), make: impl Fn(Expr) -> Expr) -> bool {
    let mut eg = holder(container);
    let holds = |x| call("Holds", vec![x]);
    let (a, b) = (expr!((A)), expr!((B)));
    let (left, right) = (holds(make(a.clone())), holds(make(b.clone())));
    let run = |eg: &mut EGraph, action| eg.run_program(vec![Command::Action(action)]).unwrap();
    run(&mut eg, Action::Expr(span!(), left.clone()));
    run(&mut eg, Action::Expr(span!(), right.clone()));
    assert!(!equal(&mut eg, left.clone(), right.clone()));
    run(&mut eg, Action::Union(span!(), a, b));
    run_ruleset(&mut eg, "").unwrap();
    check(&mut eg, vec![Fact::Eq(span!(), left, right)]);
    eg.get_size("Holds") == 1
}

#[test]
fn containers_rebuild_after_unions() {
    assert!(congruent_after_union(("Vec", &["Math"]), |x| call(
        "vec-of",
        vec![x]
    )));
    assert!(congruent_after_union(("MultiSet", &["Math"]), |x| call(
        "multiset-of",
        vec![x]
    )));
    let map = |x: Expr| call("map-insert", vec![call("map-empty", vec![]), x.clone(), x]);
    assert!(congruent_after_union(("Map", &["Math", "Math"]), map));
    assert!(congruent_after_union(
        ("Pair", &["Math", "Math"]),
        |x: Expr| call("pair", vec![x.clone(), x])
    ));
}

#[test]
fn a_set_collapses_after_a_union() {
    let mut eg = holder(("Set", &["Math"]));
    let holds = |xs| call("Holds", vec![call("set-of", xs)]);
    let (both, one) = (holds(vec![expr!((A)), expr!((B))]), holds(vec![expr!((B))]));
    eg.run_program(vec![
        Command::Action(Action::Expr(span!(), both.clone())),
        Command::Action(Action::Expr(span!(), one.clone())),
    ])
    .unwrap();
    eg.run_program(vec![Command::Action(action!((union(A)(B))))])
        .unwrap();
    check(&mut eg, vec![Fact::Eq(span!(), both.clone(), one)]);
    let set = value(&mut eg, call("set-of", vec![expr!((A)), expr!((B))]));
    let length = eg
        .value_to_container::<tir_egglog::sort::SetContainer>(set)
        .unwrap()
        .data
        .len();
    assert_eq!(length, 1, "{{A, B}} is {{B}} once A = B");
}

#[test]
fn nested_vectors_rebuild_through_the_inner_vector() {
    let mut eg = holder(("Vec", &["Math"]));
    eg.run_program(vec![Command::Sort {
        span: span!(),
        name: "CC".into(),
        presort_and_args: Some(("Vec".to_owned(), vec![exprs::var("C")])),
    }])
    .unwrap();
    let schema = tir_egglog::ast::Schema::new(vec!["CC".to_owned()], "Math".to_owned());
    add_constructor(&mut eg, "HoldsCC", schema, None, false).unwrap();
    let nested = |x| {
        call(
            "HoldsCC",
            vec![call("vec-of", vec![call("vec-of", vec![x])])],
        )
    };
    let (left, right) = (nested(expr!((A))), nested(expr!((B))));
    let run = |eg: &mut EGraph, action| eg.run_program(vec![Command::Action(action)]).unwrap();
    run(&mut eg, Action::Expr(span!(), left.clone()));
    run(&mut eg, Action::Expr(span!(), right.clone()));
    run(&mut eg, action!((union(A)(B))));
    check(&mut eg, vec![Fact::Eq(span!(), left, right)]);
    assert_eq!(eg.get_size("HoldsCC"), 1);
}

#[test]
fn vec_union_unions_the_elements() -> Result<(), Error> {
    let mut eg = holder(("Vec", &["Math"]));
    let vec_union = call(
        "vec-union",
        vec![
            call("vec-of", vec![expr!((A))]),
            call("vec-of", vec![expr!((B))]),
        ],
    );
    eg.run_program(vec![Command::Action(Action::Expr(span!(), vec_union))])?;
    check(&mut eg, vec![Fact::Eq(span!(), expr!((A)), expr!((B)))]);
    Ok(())
}

#[test]
fn colored_collections_read_local_contents_and_preserve_other_views() {
    use tir_egglog::sort::{Container, MapKind, PairKind, SetKind, VecKind};
    use tir_egglog::{Color, Merge, Rules, Schedule, SortKind};

    let mut eg = EGraph::default();
    eg.database_mut().add_sort("E", SortKind::Eq);
    for (name, presort, args) in [
        ("ES", "Set", vec!["E"]),
        ("EV", "Vec", vec!["E"]),
        ("EM", "Map", vec!["E", "i64"]),
        ("EP", "Pair", vec!["E", "i64"]),
        ("Nested", "Set", vec!["ES"]),
    ] {
        eg.run_program(vec![Command::Sort {
            span: span!(),
            name: name.to_owned(),
            presort_and_args: Some((
                presort.to_owned(),
                args.into_iter().map(exprs::var).collect(),
            )),
        }])
        .unwrap();
    }
    eg.run_program(vec![Command::Sort {
        span: span!(),
        name: "SetLength".to_owned(),
        presort_and_args: Some((
            "UnstableFn".to_owned(),
            vec![call("ES", vec![]), exprs::var("i64")],
        )),
    }])
    .unwrap();
    let db = eg.database_mut();
    let [e, es, ev, em, ep, nested, int] =
        ["E", "ES", "EV", "EM", "EP", "Nested", "i64"].map(|s| db.sort(s).unwrap());
    let seed = db.add_relation("collections", &[es, ev, em, ep, nested, e]);
    let result = db.add_function("length", &[es], int, Merge::New);
    let removed = db.add_function("removed", &[es], int, Merge::New);
    let projected = db.add_relation("projected", &[e, int]);
    let nested_length = db.add_function("nested_length", &[nested], int, Merge::New);
    let forwarded = db.add_function("forwarded", &[es], int, Merge::New);
    let local = db.add_relation("local", &[ev, e]);
    let isolated = db.add_relation("isolated", &[es, e]);
    let isolated_length = db.add_function("isolated_length", &[es], int, Merge::New);
    let mut rules = Rules::new();
    let var = exprs::var;
    let len = call("set-length", vec![var("s")]);
    let applied = call(
        "unstable-app",
        vec![
            call("unstable-fn", vec![exprs::string("set-length")]),
            var("s"),
        ],
    );
    let contains = call("vec-contains", vec![var("v"), var("x")]);
    rules
        .rule(
            db,
            "",
            facts![(collections s v m p n x) (= k (unquote applied))],
            actions![(set (forwarded s) k)],
        )
        .unwrap();
    rules
        .rule(db, "", facts![(local v x)], actions![(unquote contains)])
        .unwrap();
    let removed_len = call(
        "set-length",
        vec![call("set-remove", vec![var("s"), var("x")])],
    );
    let isolated_removed = call(
        "set-length",
        vec![call("set-remove", vec![var("s"), var("x")])],
    );
    rules
        .rule(
            db,
            "",
            facts![(isolated s x)],
            actions![(set (isolated_length s) (unquote isolated_removed))],
        )
        .unwrap();
    let nested_len = call("set-length", vec![var("n")]);
    let vec_item = call("vec-get", vec![var("v"), exprs::int(0)]);
    let map_item = call("map-get", vec![var("m"), var("x")]);
    let first = call("pair-first", vec![var("p")]);
    let second = call("pair-second", vec![var("p")]);
    rules
        .rule(
            db,
            "",
            facts![(collections s v m p n x)],
            actions![
                (set (length s) (unquote len))
                (set (removed s) (unquote removed_len))
                (set (nested_length n) (unquote nested_len))
                (projected (unquote vec_item) (unquote map_item))
                (projected (unquote first) (unquote second))
            ],
        )
        .unwrap();
    let mut program = rules.compile(db).unwrap();
    program.set_report_level(tir_egglog::ReportLevel::StageInfo);
    let [z, a, b] = [(); 3].map(|_| db.fresh(e));
    let extra = db.fresh(e);
    let seven = db.base(7i64);
    let s = db.container(Container::<SetKind>::new(vec![a, b], [true; 2]));
    let sa = db.container(Container::<SetKind>::new(vec![a], [true; 2]));
    let sb = db.container(Container::<SetKind>::new(vec![b], [true; 2]));
    let n = db.container(Container::<SetKind>::new(vec![sa, sb], [true; 2]));
    let v = db.container(Container::<VecKind>::new(vec![b], [true; 2]));
    let m = db.container(Container::<MapKind>::new(vec![b, seven], [true, false]));
    let p = db.container(Container::<PairKind>::new(vec![b, seven], [true, false]));
    db.insert(seed, &[s, v, m, p, n, a]).unwrap();
    let tail = db.container(Container::<SetKind>::new(vec![b, z], [true; 2]));
    db.insert(seed, &[tail, v, m, p, n, a]).unwrap();
    let isolated_set = db.container(Container::<SetKind>::new(vec![b, extra], [true; 2]));
    db.insert(isolated, &[isolated_set, z]).unwrap();
    let parent = db.color(&[]);
    let child = db.color(&[parent]);
    let sibling = db.color(&[]);
    program.run(db, &Schedule::saturate("")).unwrap();
    db.union_in(parent, a, b);
    db.insert_in(parent, local, &[v, a]).unwrap();
    program.run(db, &Schedule::saturate("")).unwrap();
    let read = |db: &tir_egglog::Database, c, table, key| {
        db.base_value::<i64>(db.lookup_in(c, table, &[key]).unwrap())
    };
    let check = |c, length, removed_count, nested_count| {
        assert_eq!(read(db, c, result, s), length);
        assert_eq!(read(db, c, forwarded, s), length);
        assert_eq!(read(db, c, removed, s), removed_count);
        assert_eq!(read(db, c, nested_length, n), nested_count);
    };
    for c in [parent, child] {
        check(c, 1, 0, 1);
        assert!(db.lookup_in(c, projected, &[a, seven]).is_some());
    }
    for c in [Color::BASE, sibling] {
        check(c, 2, 1, 2);
        assert_ne!(db.find_in(c, a), db.find_in(c, b));
    }
    // A container created after the local equality must join the same class.
    let late = db.container(Container::<SetKind>::new(vec![a, b, z], [true; 2]));
    let canonical = db.container(Container::<SetKind>::new(vec![a, z], [true; 2]));
    db.rebuild().unwrap();
    assert_eq!(db.find_in(parent, late), db.find_in(parent, canonical));
    db.union_in(parent, a, z);
    program.run(db, &Schedule::saturate("")).unwrap();
    assert_eq!(db.find_in(child, late), db.find_in(child, sa));
    assert_eq!(read(db, parent, isolated_length, isolated_set), 1);
    assert_eq!(read(db, child, isolated_length, isolated_set), 1);
    assert_eq!(read(db, parent, result, tail), 1);
    assert_eq!(read(db, child, forwarded, tail), 1);
    let stable = program.run(db, &Schedule::saturate("")).unwrap();
    assert!(!stable.updated);
    assert_eq!(stable.num_matches_per_rule.values().sum::<usize>(), 0);
    assert_eq!(read(db, child, result, s), 1);
    db.union(a, b);
    program.run(db, &Schedule::saturate("")).unwrap();
    assert_eq!(read(db, sibling, result, s), 1);
    assert_eq!(read(db, sibling, forwarded, s), 1);
    db.drop_color(parent);
    db.rebuild().unwrap();
    assert_eq!(db.rows_in(child, local).count(), 0);
    assert_eq!(read(db, sibling, result, s), 1);
}
