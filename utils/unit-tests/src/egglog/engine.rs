//! The engine through the e-graph: egglog's egglog-bridge/src/tests.rs on
//! `EGraph` and `Database`, the catch-up run of a rule added after a run,
//! and `:until`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tir_egglog::ast::{Command, RunConfig, Schedule, Schema};
use tir_egglog::prelude::*;
use tir_egglog::sort::VecContainer;
use tir_egglog::{ArcSort, Database, Error, Primitive, PurePrim, Read, Rules, Value};

fn schema(input: &[&str], output: &str) -> Schema {
    Schema::new(
        input.iter().map(|&s| s.to_owned()).collect(),
        output.to_owned(),
    )
}

fn run(eg: &mut EGraph, actions: Vec<Action>) -> Result<(), Error> {
    eg.run_program(actions.into_iter().map(Command::Action).collect())
        .map(drop)
}

// A ruleset `name` of one rule.
fn ruleset(eg: &mut EGraph, name: &str, facts: Facts, actions: Actions) -> Result<(), Error> {
    add_ruleset(eg, name)?;
    rule(eg, name, facts, actions).map(drop)
}

fn changed(eg: &mut EGraph, ruleset: &str) -> bool {
    eg.step_rules(ruleset).unwrap().updated
}

fn value(eg: &mut EGraph, expr: Expr) -> Value {
    eg.eval_expr(&expr).unwrap().1
}

fn same(eg: &EGraph, a: Value, b: Value) -> bool {
    eg.database().find(a) == eg.database().find(b)
}

// (Num i64) and (Add M M), the left- and right-associated sums of 0..5, and
// commutativity (only of `(Num 1)` on the left when `restricted`) and
// associativity.
fn ac_case(restricted: bool) -> Result<bool, Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype M (Num i64) (Add M M)));
    add_ruleset(&mut eg, "ac")?;
    if restricted {
        rule(
            &mut eg,
            "ac",
            facts![(= id (Add x y)) (= x (Num 1))],
            actions![(union id (Add y x))],
        )?;
    } else {
        rule(
            &mut eg,
            "ac",
            facts![(= id (Add x y))],
            actions![(union id (Add y x))],
        )?;
    }
    rule(
        &mut eg,
        "ac",
        facts![(= id (Add x (Add y z)))],
        actions![(union id (Add (Add x y) z))],
    )?;
    assert!(!changed(&mut eg, "ac"), "an empty database stays empty");
    let num = |i: i64| exprs::call("Num", vec![exprs::int(i)]);
    let add = |a, b| exprs::call("Add", vec![a, b]);
    let left = (1..5).fold(num(0), |sum, i| add(num(i), sum));
    let right = (0..4).fold(num(4), |sum, i| add(sum, num(i)));
    let (left, right) = (value(&mut eg, left), value(&mut eg, right));
    while changed(&mut eg, "ac") {}
    Ok(same(&eg, left, right))
}

#[test]
fn ac() -> Result<(), Error> {
    assert!(ac_case(false)?, "the sums reassociate");
    Ok(())
}

// Every table of a database can subsume, so this is `ac` again.
#[test]
fn ac_subsume() -> Result<(), Error> {
    assert!(ac_case(false)?);
    Ok(())
}

#[test]
fn ac_fail() -> Result<(), Error> {
    assert!(!ac_case(true)?, "commuting only (Num 1) cannot reassociate");
    Ok(())
}

fn q(n: i64) -> Expr {
    let int = |n| exprs::call("bigint", vec![exprs::int(n)]);
    exprs::call("bigrat", vec![int(n), int(1)])
}

fn math_egraph() -> Result<EGraph, Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype Math
        (Diff Math Math) (Integral Math Math) (Add Math Math) (Sub Math Math) (Mul Math Math)
        (Div Math Math) (Pow Math Math) (Ln Math) (Sqrt Math) (Sin Math) (Cos Math)));
    add_constructor(&mut eg, "Rat", schema(&["BigRat"], "Math"), None, false)?;
    add_constructor(&mut eg, "Var", schema(&["String"], "Math"), None, false)?;
    let rat = |n| exprs::call("Rat", vec![q(n)]);
    let (zero, one, two, neg1) = (rat(0), rat(1), rat(2), rat(-1));
    let mut rw = |lhs: Expr, rhs: Expr| {
        let rewrite = tir_egglog::ast::Rewrite {
            span: lhs.span(),
            lhs,
            rhs,
            conditions: Vec::new(),
            name: String::new(),
        };
        eg.run_program(vec![Command::Rewrite(String::new(), rewrite, false)])
            .map(drop)
    };
    rw(expr!((Add x y)), expr!((Add y x)))?;
    rw(expr!((Mul x y)), expr!((Mul y x)))?;
    rw(expr!((Add x (Add y z))), expr!((Add (Add x y) z)))?;
    rw(expr!((Mul x (Mul y z))), expr!((Mul (Mul x y) z)))?;
    rw(
        expr!((Sub x y)),
        expr!((Add x (Mul (unquote neg1.clone()) y))),
    )?;
    rw(expr!((Add a (unquote zero.clone()))), expr!(a))?;
    rw(expr!((Mul a (unquote zero.clone()))), zero.clone())?;
    rw(expr!((Mul a (unquote one.clone()))), expr!(a))?;
    rw(expr!((Sub x x)), zero)?;
    rw(expr!((Mul x (Add b c))), expr!((Add (Mul x b) (Mul x c))))?;
    rw(expr!((Add (Mul x a) (Mul x b))), expr!((Mul x (Add a b))))?;
    rw(expr!((Mul (Pow a b) (Pow a c))), expr!((Pow a (Add b c))))?;
    rw(expr!((Pow x (unquote one.clone()))), expr!(x))?;
    rw(expr!((Pow x (unquote two))), expr!((Mul x x)))?;
    rw(
        expr!((Diff x (Add a b))),
        expr!((Add (Diff x a) (Diff x b))),
    )?;
    rw(
        expr!((Diff x (Mul a b))),
        expr!((Add (Mul a (Diff x b)) (Mul b (Diff x a)))),
    )?;
    rw(expr!((Diff x (Sin x))), expr!((Cos x)))?;
    rw(
        expr!((Diff x (Cos x))),
        expr!((Mul (unquote neg1.clone()) (Sin x))),
    )?;
    rw(expr!((Integral (unquote one) x)), expr!(x))?;
    rw(expr!((Integral (Cos x) x)), expr!((Sin x)))?;
    rw(
        expr!((Integral (Sin x) x)),
        expr!((Mul (unquote neg1) (Cos x))),
    )?;
    rw(
        expr!((Integral (Add f g) x)),
        expr!((Add (Integral f x) (Integral g x))),
    )?;
    rw(
        expr!((Integral (Sub f g) x)),
        expr!((Sub (Integral f x) (Integral g x))),
    )?;
    rw(
        expr!((Integral (Mul a b) x)),
        expr!((Sub (Mul a (Integral b x)) (Integral (Mul (Diff x a) (Integral b x)) x))),
    )?;
    Ok(eg)
}

fn math_terms(eg: &mut EGraph) {
    let var = |name: &str| exprs::call("Var", vec![exprs::string(name)]);
    let rat = |n| exprs::call("Rat", vec![q(n)]);
    let (x, y, five) = (var("x"), var("y"), var("five"));
    let terms = [
        expr!((Integral (Ln (unquote x.clone())) (unquote x.clone()))),
        expr!((Integral (Add (unquote x.clone()) (Cos (unquote x.clone()))) (unquote x.clone()))),
        expr!((Integral (Mul (Cos (unquote x.clone())) (unquote x.clone())) (unquote x.clone()))),
        expr!((Diff (unquote x.clone()) (Add (unquote rat(1)) (Mul (unquote rat(2)) (unquote x.clone()))))),
        expr!((Diff (unquote x.clone()) (Sub (Pow (unquote x.clone()) (unquote rat(3))) (Mul (unquote rat(7)) (Pow (unquote x.clone()) (unquote rat(2))))))),
        expr!((Add (Mul (unquote y.clone()) (Add (unquote x.clone()) (unquote y))) (Sub (Add (unquote x.clone()) (unquote rat(2))) (Add (unquote x.clone()) (unquote x))))),
        expr!((Div (unquote rat(1)) (Sub (Div (Add (unquote rat(1)) (Sqrt (unquote five.clone()))) (unquote rat(2))) (Div (Sub (unquote rat(1)) (Sqrt (unquote five))) (unquote rat(2)))))),
    ];
    for term in terms {
        value(eg, term);
    }
}

// The table sizes of egglog-bridge's math test, which it checked against
// egglog: 8 iterations of the math theory on rationals.
fn math_test() -> Result<(), Error> {
    let mut eg = math_egraph()?;
    math_terms(&mut eg);
    for _ in 0..8 {
        if !changed(&mut eg, "") {
            break;
        }
    }
    let sizes = [
        ("Diff", 338),
        ("Integral", 782),
        ("Sub", 483),
        ("Div", 3),
        ("Pow", 2),
        ("Ln", 1),
        ("Sqrt", 1),
        ("Sin", 1),
        ("Cos", 1),
        ("Rat", 5),
        ("Var", 3),
        ("Add", 2977),
        ("Mul", 3516),
    ];
    for (table, size) in sizes {
        assert_eq!(eg.get_size(table), size, "rows of {table}");
    }
    Ok(())
}

#[test]
fn math() -> Result<(), Error> {
    math_test()
}

#[test]
fn math_subsume() -> Result<(), Error> {
    math_test()
}

// (num i64), (add M M), (vec MathVec): every vector pushes `last + 0` and
// `0 + last`; then evaluating additions collapses them.
#[test]
fn basic_container() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype M (Num i64) (Add M M)));
    let vec_sort = Some(("Vec".to_owned(), vec![exprs::var("M")]));
    eg.run_program(vec![Command::Sort {
        span: span!(),
        name: "MathVec".into(),
        presort_and_args: vec_sort,
    }])?;
    add_constructor(&mut eg, "Holds", schema(&["MathVec"], "M"), None, false)?;
    let call = |head: &str, args: Vec<Expr>| exprs::call(head, args);
    let holds = |x| Action::Expr(span!(), call("Holds", vec![x]));
    run(
        &mut eg,
        vec![
            holds(call("vec-empty", vec![])),
            holds(call("vec-of", vec![expr!((Num 1))])),
        ],
    )?;
    let length = call("vec-length", vec![exprs::var("v")]);
    let last = call(
        "vec-get",
        vec![exprs::var("v"), call("-", vec![length, exprs::int(1)])],
    );
    let push = |x| holds(call("vec-push", vec![exprs::var("v"), x]));
    ruleset(
        &mut eg,
        "expand",
        Facts(vec![
            fact!((Holds v)),
            Fact::Eq(span!(), exprs::var("last"), last),
        ]),
        GenericActions(vec![
            push(expr!((Add last (Num 0)))),
            push(expr!((Add (Num 0) last))),
        ]),
    )?;
    ruleset(
        &mut eg,
        "eval",
        facts![(= l (Num a)) (= r (Num b)) (= id (Add l r))],
        actions![(union id (Num (+ a b)))],
    )?;
    for vectors in [4, 8, 16] {
        assert!(changed(&mut eg, "expand"));
        assert_eq!(
            eg.get_size("Holds"),
            vectors,
            "each vector with a last element spawns two"
        );
    }
    let mut saturated = false;
    for _ in 0..20 {
        saturated = !changed(&mut eg, "eval");
        if saturated {
            break;
        }
    }
    assert!(saturated, "evaluation saturates");
    let one = value(&mut eg, expr!((Num 1)));
    let one = eg.database().find(one);
    let mut vectors = Vec::new();
    eg.constructor_enodes("Holds", |enode| {
        let vector = eg
            .value_to_container::<VecContainer>(enode.children[0])
            .unwrap();
        vectors.push(vector.data.clone());
    })?;
    vectors.sort();
    assert_eq!(vectors, (0..5).map(|n| vec![one; n]).collect::<Vec<_>>());
    Ok(())
}

// The key of the `k` row whose e-class is the one element of a vector. Like
// egglog-bridge's external function it reads a table from a seminaive
// query, which only a primitive registered for any context may.
fn match_singleton_k(eg: &mut EGraph) {
    let db = eg.database_mut();
    let (vec_sort, math, k) = (
        db.sort("MathVec").unwrap(),
        db.sort("Math").unwrap(),
        db.table("k").unwrap(),
    );
    let signature = tir_egglog::Signature {
        inputs: vec![vec_sort],
        output: math,
    };
    db.add_primitive("match_singleton_k", signature, move |ctx, args| {
        let [entry] = ctx.container_value::<VecContainer>(args[0])?.data[..] else {
            return None;
        };
        ctx.rows(k).find(|row| row[1] == entry).map(|row| row[0])
    });
}

fn query_prim_container_match(seminaive: bool, seed_canonical: bool) -> Result<bool, Error> {
    let mut eg = EGraph::default();
    eg.seminaive = seminaive;
    add_sort(&mut eg, "Math")?;
    let vec_sort = Some(("Vec".to_owned(), vec![exprs::var("Math")]));
    eg.run_program(vec![Command::Sort {
        span: span!(),
        name: "MathVec".into(),
        presort_and_args: vec_sort,
    }])?;
    for (name, input) in [("k", "Math"), ("w", "Math"), ("l", "MathVec")] {
        add_constructor(&mut eg, name, schema(&[input], "Math"), None, false)?;
    }
    add_constructor(&mut eg, "B", schema(&[], "Math"), None, false)?;
    match_singleton_k(&mut eg);
    let vec_of = |x| exprs::call("vec-of", vec![x]);
    if seed_canonical {
        run(&mut eg, vec![Action::Expr(span!(), vec_of(expr!((k(B)))))])?;
    }
    let l = value(&mut eg, exprs::call("l", vec![vec_of(expr!((w(k(B)))))]));
    let b = value(&mut eg, expr!((B)));
    rule(&mut eg, "", facts![(= id (w x))], actions![(union id x)])?;
    rule(
        &mut eg,
        "",
        facts![(= id (l v)) (= x (match_singleton_k v))],
        actions![(union id x)],
    )?;
    let mut saturated = false;
    for _ in 0..8 {
        saturated = !changed(&mut eg, "");
        if saturated {
            break;
        }
    }
    assert!(saturated, "saturates in 8 iterations");
    Ok(same(&eg, l, b))
}

#[test]
fn seminaive_query_prim_rechecks_after_rebuild() -> Result<(), Error> {
    assert!(query_prim_container_match(true, false)?);
    assert!(query_prim_container_match(false, false)?);
    Ok(())
}

#[test]
fn seminaive_query_prim_rechecks_after_preseeded_container_rebuild() -> Result<(), Error> {
    assert!(query_prim_container_match(true, true)?);
    Ok(())
}

#[test]
fn rhs_only_rule() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype M (Num i64)));
    ruleset(&mut eg, "add", facts![], actions![(Num 0) (Num 1)])?;
    assert!(changed(&mut eg, "add"));
    let mut rows = Vec::new();
    eg.constructor_enodes("Num", |enode| {
        assert!(!enode.subsumed);
        rows.push((eg.value_to_base::<i64>(enode.children[0]), enode.eclass));
    })?;
    rows.sort();
    assert_eq!(rows.iter().map(|row| row.0).collect::<Vec<_>>(), [0, 1]);
    assert_ne!(rows[0].1, rows[1].1, "two e-classes");
    Ok(())
}

#[derive(Clone)]
struct Counter(Arc<AtomicUsize>, ArcSort);

impl Primitive for Counter {
    fn name(&self) -> &str {
        "count"
    }

    fn signature(&self) -> Vec<ArcSort> {
        vec![self.1.clone()]
    }
}

impl PurePrim for Counter {
    fn apply<'a, 'db>(&self, state: tir_egglog::PureState<'a, 'db>, _: &[Value]) -> Option<Value> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some(state.base_values().get(()))
    }
}

#[test]
fn rhs_only_rule_only_runs_once() -> Result<(), Error> {
    let mut eg = EGraph::default();
    let counter = Arc::new(AtomicUsize::new(0));
    eg.add_pure_primitive(Counter(counter.clone(), UnitSort.to_arcsort()));
    ruleset(&mut eg, "count", facts![], actions![(count)])?;
    assert!(!changed(&mut eg, "count"));
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(!changed(&mut eg, "count"));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "a rule that reads no table runs once"
    );
    Ok(())
}

fn i64_rows(eg: &EGraph, name: &str) -> Vec<(i64, i64)> {
    let mut rows = Vec::new();
    eg.function_entries(name, |entry| {
        assert!(!entry.subsumed);
        rows.push((
            eg.value_to_base(entry.inputs[0]),
            eg.value_to_base(entry.output),
        ));
    })
    .unwrap();
    rows.sort();
    rows
}

#[test]
fn mergefn_arithmetic() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_function(
        &mut eg,
        "f",
        schema(&["i64"], "i64"),
        Some(expr!((+ 1 (* old new)))),
    )?;
    ruleset(
        &mut eg,
        "rule1",
        facts![],
        actions![(set (f 1) 0) (set (f 2) 1)],
    )?;
    assert!(changed(&mut eg, "rule1"));
    assert_eq!(i64_rows(&eg, "f"), [(1, 0), (2, 1)]);
    ruleset(
        &mut eg,
        "rule2",
        facts![],
        actions![(set (f 1) 5) (set (f 2) 6)],
    )?;
    assert!(changed(&mut eg, "rule2"));
    assert_eq!(
        i64_rows(&eg, "f"),
        [(1, 1), (2, 7)],
        "1 + 0 * 5 and 1 + 1 * 6"
    );
    ruleset(
        &mut eg,
        "rule3",
        facts![],
        actions![(set (f 1) 3) (set (f 2) 4)],
    )?;
    assert!(changed(&mut eg, "rule3"));
    assert_eq!(
        i64_rows(&eg, "f"),
        [(1, 4), (2, 29)],
        "1 + 1 * 3 and 1 + 7 * 4"
    );
    Ok(())
}

// The function `f` to e-classes merges as (g (g new new) (g old old)); the
// e-classes come from `(Base i)`.
#[test]
fn mergefn_nested_function() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype M (g M M) (Base i64)));
    add_function(
        &mut eg,
        "f",
        schema(&["i64"], "M"),
        Some(expr!((g (g new new) (g old old)))),
    )?;
    ruleset(
        &mut eg,
        "write",
        facts![],
        actions![(set (f 1) (Base 1)) (set (f 2) (Base 2))],
    )?;
    assert!(changed(&mut eg, "write"));
    let (base1, base2) = (
        value(&mut eg, expr!((Base 1))),
        value(&mut eg, expr!((Base 2))),
    );
    assert_eq!(eg.get_size("g"), 0, "no merge yet");
    ruleset(&mut eg, "iterate", facts![], actions![(set (f 1) (Base 2))])?;
    assert!(changed(&mut eg, "iterate"));
    assert_eq!(eg.get_size("g"), 3);
    let g = |eg: &mut EGraph, a: Value, b: Value| {
        let found = eg.update(|fs| fs.eclass_of("g", (a, b))).unwrap();
        found.expect("the merge made the row")
    };
    let (inner_new, inner_old) = (g(&mut eg, base2, base2), g(&mut eg, base1, base1));
    let merged = g(&mut eg, inner_new, inner_old);
    let f1 = eg.update(|fs| fs.lookup("f", 1_i64))?.unwrap();
    assert!(same(&eg, f1, merged));
    Ok(())
}

#[derive(Clone)]
struct IsEven(Arc<AtomicUsize>, ArcSort, ArcSort);

impl Primitive for IsEven {
    fn name(&self) -> &str {
        "is_even"
    }

    fn signature(&self) -> Vec<ArcSort> {
        vec![self.1.clone(), self.2.clone()]
    }
}

impl PurePrim for IsEven {
    fn apply<'a, 'db>(
        &self,
        state: tir_egglog::PureState<'a, 'db>,
        args: &[Value],
    ) -> Option<Value> {
        self.0.fetch_add(1, Ordering::Relaxed);
        let x: i64 = state.value_to_base(args[0]);
        Some(state.base_to_value(x % 2 == 0))
    }
}

fn copy_rows(eg: &EGraph, name: &str) -> Vec<i64> {
    let mut rows = Vec::new();
    eg.constructor_enodes(name, |enode| rows.push(eg.value_to_base(enode.children[0])))
        .unwrap();
    rows.sort();
    rows
}

#[test]
fn constrain_prims_simple() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype M (f i64) (g i64)));
    let calls = Arc::new(AtomicUsize::new(0));
    eg.add_pure_primitive(IsEven(
        calls.clone(),
        I64Sort.to_arcsort(),
        BoolSort.to_arcsort(),
    ));
    ruleset(&mut eg, "write", facts![], actions![(f 1) (f 2) (f 3)])?;
    let truth = exprs::bool(true);
    ruleset(
        &mut eg,
        "copy",
        facts![(= id (f x)) (= (unquote truth) (is_even x))],
        actions![(union (g x) id)],
    )?;
    changed(&mut eg, "write");
    assert_eq!(copy_rows(&eg, "f"), [1, 2, 3]);
    changed(&mut eg, "copy");
    let first = calls.load(Ordering::Relaxed);
    assert!(first > 0);
    assert!(!changed(&mut eg, "copy"));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        first,
        "no new rows, no new calls"
    );
    assert_eq!(copy_rows(&eg, "g"), [2]);
    Ok(())
}

#[test]
fn constrain_prims_abstract() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype M (f i64) (g i64)));
    let minus_one = exprs::int(-1);
    ruleset(
        &mut eg,
        "write",
        facts![],
        actions![(f (unquote minus_one)) (f 0) (f 1)],
    )?;
    ruleset(
        &mut eg,
        "copy",
        facts![(= id (f x)) (= n (- 0 x)) (= n (abs x))],
        actions![(union (g x) id)],
    )?;
    changed(&mut eg, "write");
    changed(&mut eg, "copy");
    assert_eq!(copy_rows(&eg, "g"), [-1, 0], "only where -x = |x|");
    Ok(())
}

#[test]
fn basic_subsumption() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype M (f i64) (g i64)));
    ruleset(&mut eg, "write", facts![], actions![(f 1) (f 2)])?;
    ruleset(
        &mut eg,
        "subsume",
        facts![],
        actions![(subsume (f 2)) (subsume (f 3))],
    )?;
    ruleset(
        &mut eg,
        "copy",
        facts![(= id (f x))],
        actions![(union (g x) id)],
    )?;
    let rows = |eg: &EGraph, name: &str| {
        let mut rows = Vec::new();
        eg.constructor_enodes(name, |enode| {
            rows.push((eg.value_to_base::<i64>(enode.children[0]), enode.subsumed))
        })
        .unwrap();
        rows.sort();
        rows
    };
    changed(&mut eg, "write");
    assert_eq!(rows(&eg, "f"), [(1, false), (2, false)]);
    changed(&mut eg, "subsume");
    assert_eq!(rows(&eg, "f"), [(1, false), (2, true), (3, true)]);
    changed(&mut eg, "copy");
    assert_eq!(rows(&eg, "g"), [(1, false)], "subsumed rows do not match");
    Ok(())
}

// A lookup in an action of a key the function lacks fails the run.
#[test]
fn lookup_failure_panics() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_function(&mut eg, "f", schema(&["i64"], "i64"), None)?;
    ruleset(
        &mut eg,
        "write",
        facts![],
        actions![(set (f 1) 1) (set (f 2) 2)],
    )?;
    eg.step_rules("write")?;
    ruleset(&mut eg, "success", facts![], actions![(f 1)])?;
    eg.step_rules("success")?;
    ruleset(&mut eg, "failure", facts![], actions![(f 3)])?;
    assert!(eg.step_rules("failure").is_err());
    Ok(())
}

// In a query, a missing key only drops the match.
#[test]
fn a_query_lookup_of_a_missing_key_drops_the_match() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_function(&mut eg, "f", schema(&["i64"], "i64"), None)?;
    add_relation(&mut eg, "item", vec!["i64".into()])?;
    add_relation(&mut eg, "seen", vec!["i64".into()])?;
    run(
        &mut eg,
        vec![
            action!((set (f 1) 10)),
            action!((item 1)),
            action!((item 3)),
        ],
    )?;
    ruleset(
        &mut eg,
        "look",
        facts![(item x) (= y (f x))],
        actions![(seen y)],
    )?;
    eg.step_rules("look")?;
    let mut seen = Vec::new();
    eg.constructor_enodes("seen", |enode| {
        seen.push(eg.value_to_base::<i64>(enode.children[0]))
    })?;
    assert_eq!(seen, [10]);
    Ok(())
}

#[derive(Clone)]
struct AssertOdd(ArcSort);

impl Primitive for AssertOdd {
    fn name(&self) -> &str {
        "assert_odd"
    }

    fn signature(&self) -> Vec<ArcSort> {
        vec![self.0.clone(), UnitSort.to_arcsort()]
    }
}

impl PurePrim for AssertOdd {
    fn apply<'a, 'db>(
        &self,
        state: tir_egglog::PureState<'a, 'db>,
        args: &[Value],
    ) -> Option<Value> {
        let x: i64 = state.value_to_base(args[0]);
        (x % 2 == 1).then(|| state.base_to_value(()))
    }
}

#[test]
fn primitive_failure_panics() -> Result<(), Error> {
    let mut eg = EGraph::default();
    eg.add_pure_primitive(AssertOdd(I64Sort.to_arcsort()));
    ruleset(
        &mut eg,
        "odd",
        facts![],
        actions![(assert_odd 1) (assert_odd 2)],
    )?;
    assert!(matches!(eg.step_rules("odd"), Err(Error::Panic(_))));
    Ok(())
}

#[test]
fn action_panic_rebuilds_before_returning() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype M (A) (B) (C) (facts M)));
    run(&mut eg, vec![action!((facts(A))), action!((facts(B)))])?;
    assert_eq!(eg.get_size("facts"), 2);
    ruleset(
        &mut eg,
        "boom",
        facts![],
        actions![(union (A) (B)) (panic "boom")],
    )?;
    let error = eg.step_rules("boom").unwrap_err();
    assert_eq!(error, Error::Panic("boom".to_owned()));
    let (a, b, c) = (
        value(&mut eg, expr!((A))),
        value(&mut eg, expr!((B))),
        value(&mut eg, expr!((C))),
    );
    assert!(same(&eg, a, b), "the union before the panic applies");
    assert_eq!(
        eg.get_size("facts"),
        1,
        "and the rows are canonical before the error"
    );
    ruleset(&mut eg, "after", facts![], actions![(union(A)(C))])?;
    eg.step_rules("after")?;
    assert!(same(&eg, a, c), "the e-graph stays usable");
    Ok(())
}

#[test]
fn panic_functions_trigger_early_stop() -> Result<(), Error> {
    let mut eg = EGraph::default();
    eg.update(|mut fs| {
        assert!(!fs.should_stop());
        assert!(fs.panic().is_none());
        assert!(fs.should_stop());
        Ok(())
    })
}

// `count` adds 1 per match, so a second match of an old row would show.
#[test]
fn a_rule_added_after_a_run_catches_up_alone() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_relation(&mut eg, "item", vec!["i64".into()])?;
    let sum = Some(expr!((+ old new)));
    add_function(&mut eg, "count", schema(&["i64"], "i64"), sum.clone())?;
    add_function(&mut eg, "later", schema(&["i64"], "i64"), sum)?;
    run(&mut eg, vec![action!((item 1)), action!((item 2))])?;
    ruleset(&mut eg, "r", facts![(item x)], actions![(set (count x) 1)])?;
    run_ruleset(&mut eg, "r")?;
    assert_eq!(i64_rows(&eg, "count"), [(1, 1), (2, 1)]);
    rule(&mut eg, "r", facts![(item x)], actions![(set (later x) 1)])?;
    run_ruleset(&mut eg, "r")?;
    assert_eq!(
        i64_rows(&eg, "count"),
        [(1, 1), (2, 1)],
        "the old rule sees no new rows"
    );
    assert_eq!(
        i64_rows(&eg, "later"),
        [(1, 1), (2, 1)],
        "the new rule matches every row once"
    );
    run_ruleset(&mut eg, "r")?;
    assert_eq!(i64_rows(&eg, "later"), [(1, 1), (2, 1)]);
    Ok(())
}

// `step` grows by one per iteration from 0; `:until` stops at 5.
fn until() -> RunConfig {
    RunConfig {
        ruleset: "grow".to_owned(),
        until: Some(facts![(step 5)].0),
    }
}

#[test]
fn until_stops_a_saturating_run() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_relation(&mut eg, "step", vec!["i64".into()])?;
    run(&mut eg, vec![action!((step 0))])?;
    ruleset(
        &mut eg,
        "grow",
        facts![(step x) (< x 100)],
        actions![(step (+ x 1))],
    )?;
    let schedule = Schedule::Saturate(span!(), Box::new(Schedule::Run(span!(), until())));
    eg.run_program(vec![Command::RunSchedule(schedule)])?;
    assert_eq!(eg.get_size("step"), 6, "steps 0 to 5");
    Ok(())
}

#[test]
fn until_plans_into_a_program() -> Result<(), Error> {
    let mut db = Database::new();
    let int = db.sort("i64")?;
    let step = db.add_relation("step", &[int]);
    let mut eg = EGraph::from_database(db);
    let mut rules = Rules::new();
    rules.rule(
        eg.database(),
        "grow",
        facts![(step x) (< x 100)],
        actions![(step (+ x 1))],
    )?;
    let schedule = Schedule::Saturate(span!(), Box::new(Schedule::Run(span!(), until())));
    rules.plan_schedule(eg.database(), &schedule)?;
    let bytes = rules.compile(eg.database())?.to_bytes();
    let db = eg.database_mut();
    let zero = db.base(0_i64);
    db.insert(step, &[zero])?;
    tir_egglog::Program::from_bytes(&bytes, db)
        .unwrap()
        .run(db, &schedule)?;
    assert_eq!(db.rows(step).count(), 6);
    Ok(())
}

// A primitive of each state, from i64 to i64.
#[derive(Clone)]
struct Op(&'static str);

impl Primitive for Op {
    fn name(&self) -> &str {
        self.0
    }

    fn signature(&self) -> Vec<ArcSort> {
        vec![I64Sort.to_arcsort(), I64Sort.to_arcsort()]
    }
}

impl tir_egglog::WritePrim for Op {
    fn apply<'a, 'db>(&self, _: tir_egglog::WriteState<'a, 'db>, args: &[Value]) -> Option<Value> {
        Some(args[0])
    }
}

impl tir_egglog::ReadPrim for Op {
    fn apply<'a, 'db>(&self, _: tir_egglog::ReadState<'a, 'db>, args: &[Value]) -> Option<Value> {
        Some(args[0])
    }
}

impl tir_egglog::FullPrim for Op {
    fn apply<'a, 'db>(&self, _: tir_egglog::FullState<'a, 'db>, args: &[Value]) -> Option<Value> {
        Some(args[0])
    }
}

// Whether a rule with `facts` and `actions` types, naive or seminaive.
fn types(eg: &mut EGraph, naive: bool, facts: Facts, actions: Actions) -> bool {
    let eval_mode = if naive {
        tir_egglog::ast::RuleEvalMode::Naive
    } else {
        tir_egglog::ast::RuleEvalMode::Seminaive
    };
    let rule = tir_egglog::ast::Rule {
        span: span!(),
        head: actions,
        body: facts.0,
        name: String::new(),
        ruleset: String::new(),
        eval_mode,
    };
    match eg.run_program(vec![Command::Rule { rule }]) {
        Ok(_) => true,
        Err(Error::Type(..)) => false,
        Err(other) => panic!("not a type error: {other}"),
    }
}

// Upstream's contexts: seminaive queries are Pure and their actions Write,
// naive queries Read and their actions Full.
#[test]
fn primitives_run_only_in_their_contexts() {
    let mut eg = EGraph::default();
    eg.add_write_primitive(Op("write_op"));
    eg.add_read_primitive(Op("read_op"));
    eg.add_full_primitive(Op("full_op"));
    add_relation(&mut eg, "item", vec!["i64".into()]).unwrap();
    add_function(&mut eg, "g", schema(&["i64"], "i64"), None).unwrap();
    assert!(
        !types(
            &mut eg,
            false,
            facts![(item x) (= y (write_op x))],
            actions![(item y)]
        ),
        "a write in a query"
    );
    assert!(
        !types(
            &mut eg,
            true,
            facts![(item x) (= y (write_op x))],
            actions![(item y)]
        ),
        "a write in a naive query"
    );
    assert!(types(
        &mut eg,
        false,
        facts![(item x)],
        actions![(set (g x) (write_op x))]
    ));
    assert!(
        !types(
            &mut eg,
            false,
            facts![(item x) (= y (read_op x))],
            actions![(item y)]
        ),
        "a read in a seminaive query"
    );
    assert!(types(
        &mut eg,
        true,
        facts![(item x) (= y (read_op x))],
        actions![(item y)]
    ));
    assert!(
        !types(
            &mut eg,
            false,
            facts![(item x)],
            actions![(item (read_op x))]
        ),
        "a read in a seminaive action"
    );
    assert!(!types(
        &mut eg,
        false,
        facts![(item x)],
        actions![(item (full_op x))]
    ));
    assert!(types(
        &mut eg,
        true,
        facts![(item x)],
        actions![(item (full_op x))]
    ));
}

// Per-rule matches and iterations are reported only above TimeOnly; the
// times of the ruleset always are.
#[test]
fn reports_follow_the_report_level() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_relation(&mut eg, "R", vec!["i64".into()])?;
    add_relation(&mut eg, "S", vec!["i64".into()])?;
    run(&mut eg, vec![action!((R 1)), action!((R 2))])?;
    ruleset(&mut eg, "copy", facts![(R x)], actions![(S x)])?;
    let report = eg.step_rules("copy")?;
    assert!(report.updated);
    assert!(report.num_matches_per_rule.is_empty() && report.iterations.is_empty());
    assert!(report.rebuild_time_per_ruleset.contains_key("copy"));
    eg.set_report_level(tir_egglog::ReportLevel::WithPlan);
    run(&mut eg, vec![action!((R 3))])?;
    let report = eg.step_rules("copy")?;
    assert_eq!(
        report
            .num_matches_per_rule
            .values()
            .copied()
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(report.iterations.len(), 1);
    Ok(())
}

#[test]
fn repeated_sets_preserve_non_idempotent_merges() {
    for (actions, expected) in [
        (actions![(set (f 1) x)], 4),
        (actions![(set (f 1) x) (set (f 1) x)], 6),
    ] {
        let mut db = Database::new();
        let int = db.sort("i64").unwrap();
        let add = db.add_primitive(
            "add",
            tir_egglog::Signature {
                inputs: vec![int, int],
                output: int,
            },
            |ctx, args| {
                let sum = ctx.base_value::<i64>(args[0]) + ctx.base_value::<i64>(args[1]);
                Some(ctx.base(sum))
            },
        );
        let f = db.add_function(
            "f",
            &[int],
            int,
            tir_egglog::Merge::Prim(add, vec![tir_egglog::Merge::Old, tir_egglog::Merge::New]),
        );
        let (one, two) = (db.base(1_i64), db.base(2_i64));
        db.insert(f, &[one, two]).unwrap();
        let mut rules = Rules::new();
        rules
            .rule(&db, "write", facts![(= x (f 1))], actions)
            .unwrap();
        let mut program = rules.compile(&db).unwrap();
        program.run(&mut db, &Schedule::run("write")).unwrap();
        assert_eq!(
            db.base_value::<i64>(db.lookup(f, &[one]).unwrap()),
            expected
        );
    }
}

fn deferred_program(
    db: &Database,
    facts: Facts,
    mut first: Actions,
    second: Actions,
    separate: bool,
) -> tir_egglog::Program {
    let mut rules = Rules::new();
    if separate {
        rules.rule(db, "first", facts.clone(), first).unwrap();
        rules.rule(db, "second", facts, second).unwrap();
        rules
            .add_combined_ruleset("run", &["first".into(), "second".into()])
            .unwrap();
    } else {
        first.0.extend(second.0);
        rules.rule(db, "run", facts, first).unwrap();
    }
    rules.compile(db).unwrap()
}

#[test]
fn deferred_relation_entries_keep_snapshot_eligibility() {
    for present in [false, true] {
        for reverse in [false, true] {
            for separate in [false, true] {
                let mut db = Database::new();
                let int = db.sort("i64").unwrap();
                let input = db.add_relation("R", &[int]);
                let target = db.add_relation("D", &[int]);
                let one = db.base(1_i64);
                db.insert(input, &[one]).unwrap();
                if present {
                    db.insert(target, &[one]).unwrap();
                }
                let (mut first, mut second) = (actions![(delete (D x))], actions![(D x)]);
                if reverse {
                    std::mem::swap(&mut first, &mut second);
                }
                let mut program = deferred_program(&db, facts![(R x)], first, second, separate);
                program.run(&mut db, &Schedule::run("run")).unwrap();
                assert_eq!(
                    db.rows(target).count(),
                    usize::from(!present),
                    "present={present} reverse={reverse} separate={separate}"
                );
            }
        }
    }
}

#[test]
fn deferred_function_sets_replace_deleted_rows() {
    for merge_kind in 0..3 {
        for old in [None, Some(7_i64)] {
            for new in [1_i64, 7] {
                for reverse in [false, true] {
                    for separate in [false, true] {
                        let mut db = Database::new();
                        let int = db.sort("i64").unwrap();
                        let input = db.add_relation("R", &[int]);
                        let add = db.add_primitive(
                            "add",
                            tir_egglog::Signature {
                                inputs: vec![int, int],
                                output: int,
                            },
                            |ctx, args| {
                                let sum =
                                    ctx.base_value::<i64>(args[0]) + ctx.base_value::<i64>(args[1]);
                                Some(ctx.base(sum))
                            },
                        );
                        let merge = match merge_kind {
                            0 => tir_egglog::Merge::New,
                            1 => tir_egglog::Merge::AssertEq,
                            _ => tir_egglog::Merge::Prim(
                                add,
                                vec![tir_egglog::Merge::Old, tir_egglog::Merge::New],
                            ),
                        };
                        let target = db.add_function("f", &[int], int, merge);
                        let one = db.base(1_i64);
                        db.insert(input, &[one]).unwrap();
                        if let Some(old) = old {
                            let old = db.base(old);
                            db.insert(target, &[one, old]).unwrap();
                        }
                        let (mut first, mut second) = (
                            actions![(delete (f x))],
                            actions![(set (f x) (unquote exprs::int(new)))],
                        );
                        if reverse {
                            std::mem::swap(&mut first, &mut second);
                        }
                        let mut program =
                            deferred_program(&db, facts![(R x)], first, second, separate);
                        program.run(&mut db, &Schedule::run("run")).unwrap();
                        assert_eq!(
                            db.lookup(target, &[one]).map(|v| db.base_value::<i64>(v)),
                            Some(new),
                            "merge={merge_kind} old={old:?} reverse={reverse} separate={separate}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn deferred_sets_bound_by_the_premise_survive_deletion() {
    for separate in [false, true] {
        let mut db = Database::new();
        let int = db.sort("i64").unwrap();
        let f = db.add_function("f", &[int], int, tir_egglog::Merge::New);
        let (one, seven) = (db.base(1_i64), db.base(7_i64));
        db.insert(f, &[one, seven]).unwrap();
        let mut program = deferred_program(
            &db,
            facts![(= y (f x))],
            actions![(delete (f x))],
            actions![(set (f x) y)],
            separate,
        );
        program.run(&mut db, &Schedule::run("run")).unwrap();
        assert_eq!(db.lookup(f, &[one]), Some(seven));
    }
}

#[test]
fn deferred_deletion_targets_precede_unions() {
    for delete_b in [false, true] {
        for set_b in [false, true] {
            let mut db = Database::new();
            let k = db.add_sort("K", tir_egglog::SortKind::Eq);
            let int = db.sort("i64").unwrap();
            let input = db.add_relation("R", &[k, k]);
            let f = db.add_function("f", &[k], int, tir_egglog::Merge::New);
            let (a, b, seven) = (db.fresh(k), db.fresh(k), db.base(7_i64));
            db.insert(input, &[a, b]).unwrap();
            db.insert(f, &[a, seven]).unwrap();
            let mut first = actions![(union a b)];
            if set_b {
                first.0.extend(actions![(set (f b) 42)].0);
            }
            let second = if delete_b {
                actions![(delete (f b))]
            } else {
                actions![(delete (f a))]
            };
            let mut program = deferred_program(&db, facts![(R a b)], first, second, true);
            program.run(&mut db, &Schedule::run("run")).unwrap();
            let expected = if set_b {
                Some(42)
            } else if delete_b {
                Some(7)
            } else {
                None
            };
            assert_eq!(
                db.lookup(f, &[a]).map(|v| db.base_value::<i64>(v)),
                expected
            );
        }
    }
}

#[test]
fn seminaive_variants_keep_each_matches_effects() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let unit = db.sort("Unit").unwrap();
    let input = db.add_relation("R", &[int]);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    db.add_primitive(
        "tick",
        tir_egglog::Signature {
            inputs: vec![],
            output: unit,
        },
        move |ctx, _| {
            seen.fetch_add(1, Ordering::SeqCst);
            Some(ctx.base(()))
        },
    );
    let add = db.add_primitive(
        "add",
        tir_egglog::Signature {
            inputs: vec![int, int],
            output: int,
        },
        |ctx, args| {
            let sum = ctx.base_value::<i64>(args[0]) + ctx.base_value::<i64>(args[1]);
            Some(ctx.base(sum))
        },
    );
    let count = db.add_function(
        "count",
        &[],
        int,
        tir_egglog::Merge::Prim(add, vec![tir_egglog::Merge::Old, tir_egglog::Merge::New]),
    );
    let mut rules = Rules::new();
    rules
        .rule(&db, "run", facts![(R x) (R y)], actions![(set (count) 1)])
        .unwrap();
    rules
        .rule(&db, "run", facts![(R x) (R y)], actions![(tick)])
        .unwrap();
    let mut program = rules.compile(&db).unwrap();
    for (n, expected) in [(1_i64, 1), (2, 4), (2, 4)] {
        let n = db.base(n);
        db.insert(input, &[n]).unwrap();
        program.run(&mut db, &Schedule::run("run")).unwrap();
        assert_eq!(
            db.lookup(count, &[]).map(|v| db.base_value::<i64>(v)),
            Some(expected)
        );
        assert_eq!(calls.load(Ordering::SeqCst), expected as usize);
    }
}

#[test]
fn deferred_function_reads_keep_the_committed_snapshot() {
    for old in [None, Some(7_i64)] {
        let mut db = Database::new();
        let int = db.sort("i64").unwrap();
        let input = db.add_relation("R", &[int]);
        let output = db.add_relation("Out", &[int]);
        let f = db.add_function("f", &[int], int, tir_egglog::Merge::New);
        let one = db.base(1_i64);
        db.insert(input, &[one]).unwrap();
        if let Some(old) = old {
            let old = db.base(old);
            db.insert(f, &[one, old]).unwrap();
        }
        let mut rules = Rules::new();
        rules
            .rule(
                &db,
                "run",
                facts![(R x)],
                actions![(set (f x) 42) (Out (f x))],
            )
            .unwrap();
        let mut program = rules.compile(&db).unwrap();
        let result = program.run(&mut db, &Schedule::run("run"));
        assert_eq!(result.is_ok(), old.is_some());
        assert_eq!(
            db.lookup(f, &[one]).map(|v| db.base_value::<i64>(v)),
            Some(42)
        );
        let values: Vec<i64> = db.rows(output).map(|r| db.base_value(r[0])).collect();
        assert_eq!(values, old.into_iter().collect::<Vec<_>>());
    }
}

#[test]
fn deferred_constructor_entries_share_only_missing_keys() {
    for present in [false, true] {
        let mut db = Database::new();
        let int = db.sort("i64").unwrap();
        let k = db.add_sort("K", tir_egglog::SortKind::Eq);
        let input = db.add_relation("R", &[int]);
        let constructor = db.add_constructor("C", &[int], k);
        let (one, two) = (db.base(1_i64), db.base(2_i64));
        db.insert(input, &[one]).unwrap();
        db.insert(input, &[two]).unwrap();
        if present {
            db.entry(constructor, &[one]).unwrap();
        }
        let mut program = deferred_program(
            &db,
            facts![(R x)],
            actions![(delete (C 1))],
            actions![(C 1)],
            false,
        );
        program.run(&mut db, &Schedule::run("run")).unwrap();
        assert_eq!(db.rows(constructor).count(), usize::from(!present));
    }
}

#[test]
fn deferred_deletion_removes_each_symmetric_row_once() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let target = db.add_relation("D", &[int, int]);
    let (one, two) = (db.base(1_i64), db.base(2_i64));
    db.insert(target, &[one, two]).unwrap();
    let mut rules = Rules::new();
    rules
        .rule(&db, "sym", facts![(D x y)], actions![(D y x)])
        .unwrap();
    rules.set_mode("sym", tir_egglog::Mode::Relaxed);
    rules
        .rule(
            &db,
            "run",
            facts![(D x y)],
            actions![(delete (D x y)) (delete (D y x))],
        )
        .unwrap();
    let mut program = rules.compile(&db).unwrap();
    program.run(&mut db, &Schedule::run("run")).unwrap();
    assert_eq!(db.rows(target).count(), 0);
}

#[test]
fn union_rejects_base_values_before_execution() -> Result<(), Error> {
    for action in [
        action!((union 1 2)),
        action!((union 1 1)),
        Action::Union(span!(), exprs::string("a"), exprs::string("b")),
    ] {
        let mut eg = EGraph::default();
        assert!(matches!(run(&mut eg, vec![action]), Err(Error::Type(..))));
        assert!(matches!(
            rule(&mut eg, "", facts![], actions![(union 1 2)]),
            Err(Error::Type(..))
        ));
    }
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype Node (Num i64)));
    run(&mut eg, vec![action!((union (Num 1) (Num 2)))])?;
    eg.run_program(vec![Command::Check(span!(), facts![(= (Num 1) (Num 2))].0)])?;
    eg.run_program(vec![Command::Sort {
        span: span!(),
        name: "IVec".into(),
        presort_and_args: Some(("Vec".into(), vec![exprs::var("i64")])),
    }])?;
    let vector = |n| exprs::call("vec-of", vec![exprs::int(n)]);
    run(&mut eg, vec![Action::Union(span!(), vector(1), vector(2))])?;
    eg.run_program(vec![Command::Check(
        span!(),
        vec![Fact::Eq(span!(), vector(1), vector(2))],
    )])?;
    Ok(())
}

#[test]
fn steps_follow_the_seminaive_setting_after_recompilation() -> Result<(), Error> {
    for combined in [false, true] {
        let mut eg = EGraph::default();
        add_relation(&mut eg, "R", vec!["i64".into()])?;
        add_relation(&mut eg, "Unused", vec!["i64".into()])?;
        add_function(
            &mut eg,
            "count",
            Schema::new(vec![], "i64".into()),
            Some(expr!((+ old new))),
        )?;
        ruleset(&mut eg, "one", facts![(R x)], actions![(set (count) 1)])?;
        let set = if combined {
            add_ruleset(&mut eg, "two")?;
            eg.run_program(vec![Command::UnstableCombinedRuleset(
                span!(),
                "both".into(),
                vec!["one".into(), "two".into()],
            )])?;
            "both"
        } else {
            "one"
        };
        run(&mut eg, vec![action!((R 7)), action!((set (count) 0))])?;
        eg.seminaive = false;
        for expected in 1..=2 {
            eg.step_rules(set)?;
            let (_, count) = eg.eval_expr(&expr!((count)))?;
            assert_eq!(eg.value_to_base::<i64>(count), expected);
        }
        rule(&mut eg, "one", facts![(Unused x)], actions![(R x)])?;
        eg.step_rules(set)?;
        let (_, count) = eg.eval_expr(&expr!((count)))?;
        assert_eq!(eg.value_to_base::<i64>(count), 3);
        eg.seminaive = true;
        eg.step_rules(set)?;
        let (_, count) = eg.eval_expr(&expr!((count)))?;
        assert_eq!(eg.value_to_base::<i64>(count), 3);
    }
    Ok(())
}
