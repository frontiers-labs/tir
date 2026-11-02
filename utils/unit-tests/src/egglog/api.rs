//! The Rust API of the e-graph: egglog's tests/api_const_fold.rs,
//! api_fact_ops.rs, api_query.rs, api_introspection.rs, terms.rs,
//! user_defined_command_output.rs and the prelude tests, with the schemas
//! built through the prelude in place of program text.

use std::any::TypeId;
use std::fmt::Display;
use std::sync::Arc;

use tir_egglog::ast::{Command, Rule, RuleEvalMode, Schema};
use tir_egglog::prelude::*;
use tir_egglog::sort::{MapContainer, VecContainer};
use tir_egglog::{
    ApiError, ArcSort, Error, FunctionSubtype, Primitive, RawValues, Read, ReadPrim, TermDag,
    UserDefinedCommandOutput, Value,
};

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

fn global(name: &str, expr: Expr) -> Action {
    Action::Let(span!(), name.to_owned(), expr)
}

// (function f (i64) i64 :no-merge)
fn with_function() -> EGraph {
    let mut eg = EGraph::default();
    add_function(&mut eg, "f", schema(&["i64"], "i64"), None).unwrap();
    eg
}

// (datatype List (Cons i64 List) (Nil))
fn with_list() -> Result<EGraph, Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype List (Cons i64 List) (Nil)));
    Ok(eg)
}

// (datatype Expr (Num i64) (Add Expr Expr)) and the rust rule that folds
// `(Add (Num a) (Num b))` into `(Num a+b)`.
fn const_fold() -> Result<EGraph, Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype Expr (Num i64) (Add Expr Expr)));
    add_ruleset(&mut eg, "const_fold")?;
    let expr = eg.get_sort_by_name("Expr").unwrap().clone();
    rust_rule(
        &mut eg,
        "fold_add",
        "const_fold",
        vars![sum: (expr.clone()), lhs: (expr.clone()), rhs: (expr), a: i64, b: i64],
        facts![(= sum (Add lhs rhs)) (= lhs (Num a)) (= rhs (Num b))],
        move |mut ctx, vals| {
            let [sum, _, _, a, b] = vals else {
                unreachable!()
            };
            let (a, b) = (ctx.value_to_base::<i64>(*a), ctx.value_to_base::<i64>(*b));
            let folded = ctx.add("Num", a + b).ok()?;
            ctx.union(*sum, folded).ok()?;
            Some(())
        },
    )?;
    Ok(eg)
}

#[test]
fn const_fold_collapses_addition_chain() -> Result<(), Error> {
    let mut eg = const_fold()?;
    let root = eg.update(|mut fs| -> Result<Value, Error> {
        let n2 = fs.add("Num", 2_i64)?;
        let n3 = fs.add("Num", 3_i64)?;
        let n4 = fs.add("Num", 4_i64)?;
        let inner = fs.add("Add", RawValues(vec![n3, n4]))?;
        fs.add("Add", RawValues(vec![n2, inner]))
    })?;
    for _ in 0..4 {
        run_ruleset(&mut eg, "const_fold")?;
    }
    let root = eg.database().find(root);
    let nine = eg.update(|fs| fs.eclass_of("Num", 9_i64))?;
    assert_eq!(nine, Some(root), "(Num 9) is in the class of the root");
    let nine_value = eg.base_to_value::<i64>(9);
    let mut found = false;
    eg.update(|fs| {
        fs.constructor_enodes_while("Num", |enode| {
            found = enode.eclass == root && enode.children[0] == nine_value;
            !found
        })
    })?;
    assert!(found, "the root has a (Num 9) node");
    Ok(())
}

#[test]
fn const_fold_is_a_no_op_when_no_pair_of_nums() -> Result<(), Error> {
    let mut eg = const_fold()?;
    let root = eg.update(|mut fs| fs.add("Num", 7_i64))?;
    let sizes = |eg: &mut EGraph| {
        let sizes = eg.update(|fs| {
            Ok(fs
                .table_sizes()
                .into_iter()
                .map(|(n, s)| (n.to_owned(), s))
                .collect::<Vec<_>>())
        });
        sizes.unwrap()
    };
    let before = sizes(&mut eg);
    for _ in 0..3 {
        run_ruleset(&mut eg, "const_fold")?;
    }
    assert_eq!(before, sizes(&mut eg), "no rule fired");
    assert_eq!(eg.update(|fs| fs.eclass_of("Num", 7_i64))?, Some(root));
    Ok(())
}

#[test]
fn set_then_lookup_function() -> Result<(), Error> {
    let mut eg = with_function();
    eg.update(|mut fs| fs.set("f", (1_i64,), 42_i64))?;
    let got = eg.update(|fs| fs.lookup("f", 1_i64))?;
    assert_eq!(got.map(|v| eg.value_to_base::<i64>(v)), Some(42));
    assert_eq!(eg.update(|fs| fs.lookup("f", 999_i64))?, None);
    Ok(())
}

#[test]
fn contains_and_remove_function_rows() -> Result<(), Error> {
    let mut eg = with_function();
    eg.update(|mut fs| fs.set("f", (1_i64,), 42_i64))?;
    let both = eg.update(|fs| Ok((fs.contains("f", 1_i64)?, fs.contains("f", 999_i64)?)))?;
    assert_eq!(both, (true, false));
    eg.update(|mut fs| fs.remove("f", 1_i64))?;
    assert!(!eg.update(|fs| fs.contains("f", 1_i64))?);
    eg.update(|mut fs| fs.remove("f", 1_i64))?;
    assert!(!eg.update(|fs| fs.contains("f", 1_i64))?);
    Ok(())
}

#[test]
fn relation_add_contains_and_remove() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_relation(&mut eg, "R", vec!["i64".into(), "i64".into()])?;
    eg.update(|mut fs| {
        fs.add("R", (1_i64, 2_i64))?;
        fs.add("R", (3_i64, 4_i64)).map(drop)
    })?;
    let has = |eg: &mut EGraph, key: (i64, i64)| eg.update(|fs| fs.contains("R", key)).unwrap();
    assert!(has(&mut eg, (1, 2)) && has(&mut eg, (3, 4)));
    assert!(!has(&mut eg, (1, 3)) && !has(&mut eg, (2, 1)));
    eg.update(|mut fs| fs.remove("R", (1_i64, 2_i64)))?;
    assert!(!has(&mut eg, (1, 2)) && has(&mut eg, (3, 4)));
    Ok(())
}

#[test]
fn constructor_add_returns_one_id_per_key() -> Result<(), Error> {
    let mut eg = with_list()?;
    let (cons, cons2, nil) = eg.update(|mut fs| {
        let nil = fs.add("Nil", RawValues(vec![]))?;
        let cons = fs.add("Cons", (1_i64, nil))?;
        Ok((cons, fs.add("Cons", (1_i64, nil))?, nil))
    })?;
    assert_eq!(cons, cons2);
    let present = eg.update(|fs| {
        Ok((
            fs.contains("Nil", RawValues(vec![]))?,
            fs.contains("Cons", (1_i64, nil))?,
        ))
    })?;
    assert_eq!(present, (true, true));
    let found = eg.update(|fs| {
        Ok((
            fs.eclass_of("Cons", (1_i64, nil))?,
            fs.eclass_of("Cons", (99_i64, nil))?,
        ))
    })?;
    assert_eq!(found, (Some(cons), None));
    Ok(())
}

#[test]
fn subsume_predicts_constructor_rows() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype Math (Num i64)));
    eg.update(|mut fs| fs.subsume("Num", 0_i64))?;
    let direct = eg
        .update(|fs| fs.eclass_of("Num", 0_i64))?
        .expect("subsume makes the row");
    let (add_then, subsume_then) = eg.update(|mut fs| {
        assert_eq!(fs.add("Num", 0_i64)?, direct);
        let add_then = fs.add("Num", 1_i64)?;
        fs.subsume("Num", 1_i64)?;
        assert_eq!(fs.add("Num", 1_i64)?, add_then);
        fs.subsume("Num", 2_i64)?;
        let subsume_then = fs.add("Num", 2_i64)?;
        assert_eq!(fs.add("Num", 2_i64)?, subsume_then);
        Ok((add_then, subsume_then))
    })?;
    let mut rows = Vec::new();
    eg.constructor_enodes("Num", |enode| {
        rows.push((
            eg.value_to_base::<i64>(enode.children[0]),
            enode.eclass,
            enode.subsumed,
        ));
    })?;
    rows.sort_unstable_by_key(|row| row.0);
    assert_eq!(
        rows,
        vec![
            (0, direct, true),
            (1, add_then, true),
            (2, subsume_then, true)
        ]
    );
    Ok(())
}

fn message<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn misuse_of_a_table_is_reported() -> Result<(), Error> {
    let mut eg = with_list()?;
    let err = message(eg.update(|fs| fs.lookup("Cons", (1_i64, 0_i64))));
    assert!(
        err.contains("Cons") && err.contains("constructor"),
        "got: {err}"
    );
    let err = message(eg.update(|mut fs| fs.set("Nil", RawValues(vec![]), 0_i64)));
    assert!(
        err.contains("Nil") && err.contains("constructor"),
        "got: {err}"
    );
    let err = message(eg.update(|mut fs| fs.add("Cons", 1_i64)));
    assert!(err.contains("expected 2 input"), "got: {err}");
    let mut eg = with_function();
    let err = message(eg.update(|fs| fs.eclass_of("f", 1_i64)));
    assert!(
        err.contains("`f`") && err.contains("function"),
        "got: {err}"
    );
    let err = message(eg.update(|mut fs| fs.add("f", 1_i64)));
    assert!(
        err.contains("`f`") && err.contains("function"),
        "got: {err}"
    );
    let err = message(eg.update(|mut fs| fs.set("f", (1_i64, 2_i64), 42_i64)));
    assert!(err.contains("expected 1 input"), "got: {err}");
    assert!(eg.update(|mut fs| fs.set("nope", (1_i64,), 2_i64)).is_err());
    assert!(eg.update(|fs| fs.lookup("nope", 1_i64)).is_err());
    Ok(())
}

#[test]
fn union_of_a_value_with_itself_and_empty_scans() -> Result<(), Error> {
    let mut eg = with_list()?;
    let nil = eg.update(|mut fs| fs.add("Nil", RawValues(vec![])))?;
    eg.update(|mut fs| fs.union(nil, nil))?;
    let mut count = 0;
    eg.update(|fs| fs.constructor_enodes("Cons", |_| count += 1))?;
    assert_eq!(count, 0);
    Ok(())
}

#[test]
fn set_replaces_and_wide_and_string_keys_work() -> Result<(), Error> {
    let mut eg = with_function();
    eg.update(|mut fs| fs.set("f", (5_i64,), 50_i64))?;
    let got = eg.update(|fs| fs.lookup("f", 5_i64))?;
    assert_eq!(got.map(|v| eg.value_to_base::<i64>(v)), Some(50));
    add_function(&mut eg, "g", schema(&["i64", "i64", "i64"], "i64"), None)?;
    eg.update(|mut fs| fs.set("g", (1_i64, 2_i64, 3_i64), 7_i64))?;
    let (v, has) = eg.update(|fs| {
        let v = fs
            .lookup("g", (1_i64, 2_i64, 3_i64))?
            .map(|v| fs.value_to_base::<i64>(v));
        Ok((v, fs.contains("g", (1_i64, 2_i64, 3_i64))?))
    })?;
    assert_eq!((v, has), (Some(7), true));
    add_function(&mut eg, "name-length", schema(&["String"], "i64"), None)?;
    eg.update(|mut fs| fs.set("name-length", ("hello".to_string(),), 5_i64))?;
    let got = eg.update(|fs| fs.lookup("name-length", "hello".to_string()))?;
    assert_eq!(got.map(|v| eg.value_to_base::<i64>(v)), Some(5));
    Ok(())
}

#[test]
fn function_entries_visit_each_row() -> Result<(), Error> {
    let mut eg = with_function();
    run(
        &mut eg,
        vec![
            action!((set (f 1) 42)),
            action!((set (f 2) 43)),
            action!((set (f 7) 100)),
        ],
    )?;
    let mut rows = Vec::new();
    eg.function_entries("f", |entry| {
        rows.push((
            eg.value_to_base::<i64>(entry.inputs[0]),
            eg.value_to_base::<i64>(entry.output),
        ));
    })?;
    rows.sort();
    assert_eq!(rows, vec![(1, 42), (2, 43), (7, 100)]);
    add_function(&mut eg, "h", schema(&["i64"], "i64"), None)?;
    let mut count = 0;
    eg.function_entries("h", |_| count += 1)?;
    assert_eq!(count, 0);
    let err = message(eg.function_entries("nonexistent", |_| {}));
    assert!(err.contains("nonexistent"), "got: {err}");
    let eg = with_list()?;
    let err = message(eg.function_entries("Cons", |_| {}));
    assert!(
        err.contains("Cons") && err.contains("constructor"),
        "got: {err}"
    );
    Ok(())
}

#[test]
fn query_binds_the_variables_of_each_match() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_relation(&mut eg, "R", vec!["i64".into()])?;
    run(
        &mut eg,
        vec![action!((R 1)), action!((R 2)), action!((R 7))],
    )?;
    let mut xs: Vec<i64> = eg
        .query(vars![x: i64], facts![(R x)])?
        .iter()
        .map(|m| eg.value_to_base(m["x"]))
        .collect();
    xs.sort();
    assert_eq!(xs, [1, 2, 7]);
    let mut count = 0;
    eg.constructor_enodes("R", |enode| {
        assert_eq!(
            enode.children.len(),
            1,
            "a relation row has its columns as children"
        );
        count += 1;
    })?;
    assert_eq!(count, 3);

    add_relation(&mut eg, "S", vec!["i64".into(), "i64".into()])?;
    run(&mut eg, vec![action!((S 1 2))])?;
    assert_eq!(eg.query(vars![], facts![(S 1 2)])?.len(), 1);
    assert_eq!(eg.query(vars![], facts![(S 5 5)])?.len(), 0);

    add_function(&mut eg, "f", schema(&["i64"], "i64"), None)?;
    run(
        &mut eg,
        vec![
            action!((set (f 1) 10)),
            action!((set (f 2) 20)),
            action!((set (f 3) 30)),
        ],
    )?;
    let found = eg.query(vars![x: i64, y: i64], facts![(= (f x) y)])?;
    let mut rows: Vec<(i64, i64)> = found
        .iter()
        .map(|m| (eg.value_to_base(m["x"]), eg.value_to_base(m["y"])))
        .collect();
    rows.sort();
    assert_eq!(rows, [(1, 10), (2, 20), (3, 30)]);
    Ok(())
}

#[test]
fn constructor_enodes_see_children_and_subsumption() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_sort(&mut eg, "Math")?;
    add_constructor(&mut eg, "Add", schema(&["i64", "i64"], "Math"), None, false)?;
    run(
        &mut eg,
        vec![
            global("$a", expr!((Add 1 2))),
            global("$b", expr!((Add 3 4))),
        ],
    )?;
    let mut pairs = Vec::new();
    eg.constructor_enodes("Add", |enode| {
        pairs.push((
            eg.value_to_base::<i64>(enode.children[0]),
            eg.value_to_base::<i64>(enode.children[1]),
        ));
    })?;
    pairs.sort();
    assert_eq!(pairs, [(1, 2), (3, 4)]);

    datatype!(&mut eg, (datatype Num (N i64)));
    run(
        &mut eg,
        vec![action!((N 1)), action!((N 2)), action!((subsume (N 1)))],
    )?;
    let mut rows = Vec::new();
    eg.constructor_enodes("N", |enode| {
        rows.push((eg.value_to_base::<i64>(enode.children[0]), enode.subsumed))
    })?;
    rows.sort();
    assert_eq!(
        rows,
        [(1, true), (2, false)],
        "subsumed rows are still visited"
    );
    Ok(())
}

// (datatype Math (Num i64) (Var String) (Add Math Math)) and
// (function cost (Math) i64 :no-merge).
fn math() -> Result<EGraph, Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype Math (Num i64) (Var String) (Add Math Math)));
    add_function(&mut eg, "cost", schema(&["Math"], "i64"), None)?;
    Ok(eg)
}

#[test]
fn enodes_of_an_eclass_agree_with_a_filtered_scan() -> Result<(), Error> {
    let mut eg = math()?;
    let (a, b) = (exprs::var("$a"), exprs::var("$b"));
    run(
        &mut eg,
        vec![
            global("$a", expr!((Add (Num 1) (Num 2)))),
            global("$b", expr!((Add (Num 2) (Num 1)))),
            Action::Union(span!(), a, b),
            action!((subsume (Add (Num 1) (Num 2)))),
            global("$c", expr!((Add (Num 9) (Num 9)))),
        ],
    )?;
    let mut eclasses = Vec::new();
    eg.constructor_enodes("Add", |enode| eclasses.push(enode.eclass))?;
    eclasses.sort();
    eclasses.dedup();
    assert_eq!(eclasses.len(), 2, "one class of two nodes and one of one");
    let mut total = 0;
    for eclass in eclasses {
        let mut scanned = Vec::new();
        eg.constructor_enodes("Add", |enode| {
            if enode.eclass == eclass {
                scanned.push((enode.children.to_vec(), enode.subsumed));
            }
        })?;
        let mut indexed = Vec::new();
        eg.read(|state| {
            state.constructor_enodes_for_eclass("Add", eclass, |enode| {
                indexed.push((enode.children.to_vec(), enode.subsumed))
            })
        })?;
        scanned.sort();
        indexed.sort();
        assert_eq!(indexed, scanned);
        total += indexed.len();
    }
    assert_eq!(Some(total), eg.read(|state| state.table_size("Add")));
    let err = eg
        .read(|state| state.constructor_enodes_for_eclass("cost", Value::new(0), |_| {}))
        .unwrap_err();
    assert!(
        matches!(err, Error::ApiError(ApiError::WrongSubtype { .. })),
        "got {err}"
    );
    Ok(())
}

#[test]
fn eclass_enodes_span_every_constructor() -> Result<(), Error> {
    let mut eg = math()?;
    add_constructor(&mut eg, "Neg", schema(&["Math"], "Math"), None, false)?;
    datatype!(&mut eg, (datatype Other (Wrap Math)));
    let a = exprs::var("$a");
    run(
        &mut eg,
        vec![
            global("$a", expr!((Add (Num 1) (Num 2)))),
            Action::Union(span!(), a.clone(), expr!((Neg (Num 3)))),
            global("$unrelated", exprs::call("Wrap", vec![a])),
        ],
    )?;
    let eclass = eg.eval_expr(&exprs::var("$a"))?.1;
    let mut found = Vec::new();
    eg.read(|state| {
        state.eclass_enodes(eclass, |enode| {
            assert_eq!(enode.eclass, eclass);
            found.push(enode.name.to_owned());
        })
    })?;
    found.sort();
    assert_eq!(found, ["Add", "Neg"]);
    Ok(())
}

#[test]
fn schema_accessors_report_the_declaration() -> Result<(), Error> {
    let eg = math()?;
    eg.read(|state| {
        let add = state.constructor_schema("Add").unwrap();
        let input: Vec<&str> = add.input.iter().map(|sort| sort.name()).collect();
        assert_eq!(input, ["Math", "Math"]);
        assert_eq!(add.output.name(), "Math");
        assert_eq!(state.function_schema("cost").unwrap().output.name(), "i64");
        assert!(matches!(
            state.constructor_schema("cost"),
            Err(Error::ApiError(ApiError::WrongSubtype { .. }))
        ));
        assert!(matches!(
            state.function_schema("Add"),
            Err(Error::ApiError(ApiError::WrongSubtype { .. }))
        ));
        assert!(matches!(
            state.constructor_schema("nonesuch"),
            Err(Error::ApiError(ApiError::MissingTable { .. }))
        ));
        assert_eq!(
            state.table_subtype("Add"),
            Some(FunctionSubtype::Constructor)
        );
        assert_eq!(state.table_subtype("cost"), Some(FunctionSubtype::Custom));
        assert_eq!(state.table_subtype("nonesuch"), None);
    });
    Ok(())
}

#[test]
fn map_container_remaps_contents() -> Result<(), Error> {
    let mut eg = math()?;
    let (one, two) = (exprs::var("$one"), exprs::var("$two"));
    let vec_of = |xs: Vec<Expr>| exprs::call("vec-of", xs);
    eg.run_program(vec![Command::Sort {
        span: span!(),
        name: "MathVec".into(),
        presort_and_args: Some(("Vec".into(), vec![exprs::var("Math")])),
    }])?;
    run(
        &mut eg,
        vec![
            global("$one", expr!((Num 1))),
            global("$two", expr!((Num 2))),
            global("$vec", vec_of(vec![one.clone(), two.clone()])),
            global("$swapped", vec_of(vec![two, one])),
        ],
    )?;
    let value = |eg: &mut EGraph, name: &str| eg.eval_expr(&exprs::var(name)).unwrap().1;
    let (one, two, vec, swapped) = (
        value(&mut eg, "$one"),
        value(&mut eg, "$two"),
        value(&mut eg, "$vec"),
        value(&mut eg, "$swapped"),
    );
    let swap = move |v: Value| {
        if v == one {
            two
        } else if v == two {
            one
        } else {
            v
        }
    };
    let rebuilt =
        eg.update(|mut state| Ok(state.map_container(TypeId::of::<VecContainer>(), vec, &swap)))?;
    assert_eq!(rebuilt, Some(swapped));
    let unchanged =
        eg.update(|mut state| Ok(state.map_container(TypeId::of::<VecContainer>(), vec, &|v| v)))?;
    assert_eq!(unchanged, Some(vec));
    let other =
        eg.update(|mut state| Ok(state.map_container(TypeId::of::<MapContainer>(), vec, &|v| v)))?;
    assert_eq!(other, None);
    Ok(())
}

// A read primitive giving the arity of a constructor, and one giving how
// many tables it sees.
#[derive(Clone)]
struct ConstructorArity(bool);

impl Primitive for ConstructorArity {
    fn name(&self) -> &str {
        if self.0 {
            "constructor_arity"
        } else {
            "table_count"
        }
    }

    fn signature(&self) -> Vec<ArcSort> {
        vec![StringSort.to_arcsort(), I64Sort.to_arcsort()]
    }
}

impl ReadPrim for ConstructorArity {
    fn apply<'a, 'db>(
        &self,
        state: tir_egglog::ReadState<'a, 'db>,
        args: &[Value],
    ) -> Option<Value> {
        let n = if self.0 {
            let name: String = state.base_values().unwrap(args[0]);
            state.constructor_schema(&name).ok()?.input.len()
        } else {
            state.table_sizes().len()
        };
        Some(state.base_values().get(i64::try_from(n).ok()?))
    }
}

fn naive_rule(actions: Actions) -> Command {
    let rule = Rule {
        span: span!(),
        head: actions,
        body: Vec::new(),
        name: String::new(),
        ruleset: String::new(),
        eval_mode: RuleEvalMode::Naive,
    };
    Command::Rule { rule }
}

#[test]
fn a_read_primitive_sees_the_declarations_inside_a_rule() -> Result<(), Error> {
    let mut eg = math()?;
    eg.add_read_primitive(ConstructorArity(true));
    eg.add_read_primitive(ConstructorArity(false));
    add_function(&mut eg, "arity_of", schema(&["String"], "i64"), None)?;
    add_function(&mut eg, "count_of", schema(&["String"], "i64"), None)?;
    let (add, x) = (exprs::string("Add"), exprs::string("x"));
    eg.run_program(vec![
        naive_rule(actions![(set (arity_of (unquote add.clone())) (constructor_arity (unquote add.clone())))]),
        naive_rule(actions![(set (count_of (unquote x.clone())) (table_count (unquote x)))]),
    ])?;
    run_ruleset(&mut eg, "")?;
    eg.run_program(vec![Command::Check(
        span!(),
        facts![(= (arity_of (unquote add)) 2)].0,
    )])?;
    let count = eg
        .update(|fs| fs.lookup("count_of", "x"))?
        .map(|v| eg.value_to_base::<i64>(v));
    assert_eq!(count, Some(6), "Num Var Add cost arity_of count_of");
    Ok(())
}

#[test]
fn termdag_prints_and_rejects_foreign_ids() {
    let mut td = TermDag::default();
    let x = td.var("x".into());
    let seven = td.lit(7.into());
    let f = td.app("f".into(), vec![x, seven]);
    assert_eq!(td.to_string(f), "(f x 7)");
    let before = td.clone();
    let y = td.var("y".into());
    let g = td.app("f".into(), vec![x, y]);
    assert_eq!(td.to_string(g), "(f x y)");
    let foreign = std::panic::catch_unwind(|| before.to_string(g));
    assert!(foreign.is_err(), "an id of another dag panics");
}

#[derive(Debug)]
struct BorrowedOutput<'a>(&'a str);

impl Display for BorrowedOutput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

#[test]
fn user_defined_command_output_downcasts() {
    let output: Arc<dyn UserDefinedCommandOutput> = Arc::new(String::from("structured output"));
    let output = output.as_ref().as_any();
    assert_eq!(
        output.downcast_ref::<String>().map(String::as_str),
        Some("structured output")
    );
    assert!(output.downcast_ref::<usize>().is_none());
    fn accepts<T: UserDefinedCommandOutput>(_: &T) {}
    let text = String::from("borrowed output");
    accepts(&BorrowedOutput(&text));
}

// (function fib (i64) i64 :no-merge), its first two values, and the rule
// that adds the next one, run 10 times: fib 0 to fib 11.
fn fib() -> Result<EGraph, Error> {
    let mut eg = EGraph::default();
    add_function(&mut eg, "fib", schema(&["i64"], "i64"), None)?;
    run(
        &mut eg,
        vec![action!((set (fib 0) 0)), action!((set (fib 1) 1))],
    )?;
    rule(
        &mut eg,
        "",
        facts![(= f0 (fib x)) (= f1 (fib (+ x 1)))],
        actions![(set (fib (+ x 2)) (+ f0 f1))],
    )?;
    for _ in 0..10 {
        run_ruleset(&mut eg, "")?;
    }
    Ok(eg)
}

fn fib_of(eg: &mut EGraph, n: i64) -> Vec<i64> {
    let n = exprs::int(n);
    let found = eg
        .query(vars![f: i64], facts![(= (fib (unquote n)) f)])
        .unwrap();
    found.iter().map(|m| eg.value_to_base(m["f"])).collect()
}

#[test]
fn prelude_query_and_rules() -> Result<(), Error> {
    let mut eg = fib()?;
    let found = eg.query(vars![x: i64, y: i64], facts![(= (fib x) y) (= y 13)])?;
    assert_eq!(found.len(), 1);
    assert_eq!(
        (
            eg.value_to_base::<i64>(found[0]["x"]),
            eg.value_to_base::<i64>(found[0]["y"])
        ),
        (7, 13)
    );
    assert!(fib_of(&mut eg, 20).is_empty());
    add_ruleset(&mut eg, "custom")?;
    rule(
        &mut eg,
        "custom",
        facts![(= f0 (fib x)) (= f1 (fib (+ x 1)))],
        actions![(set (fib (+ x 2)) (+ f0 f1))],
    )?;
    for _ in 0..10 {
        run_ruleset(&mut eg, "custom")?;
    }
    assert_eq!(fib_of(&mut eg, 20), [6765]);
    Ok(())
}

#[test]
fn prelude_rust_rule() -> Result<(), Error> {
    let mut eg = fib()?;
    add_ruleset(&mut eg, "custom")?;
    rust_rule(
        &mut eg,
        "demo_rule",
        "custom",
        vars![x: i64, f0: i64, f1: i64],
        facts![(= f0 (fib x)) (= f1 (fib (+ x 1)))],
        |mut ctx, values| {
            let [x, f0, f1] = values else { unreachable!() };
            let (x, f0, f1) = (
                ctx.value_to_base::<i64>(*x),
                ctx.value_to_base::<i64>(*f0),
                ctx.value_to_base::<i64>(*f1),
            );
            ctx.set("fib", (x + 2,), f0 + f1).ok()
        },
    )?;
    for _ in 0..10 {
        run_ruleset(&mut eg, "custom")?;
    }
    assert_eq!(fib_of(&mut eg, 20), [6765]);
    Ok(())
}

#[test]
fn rust_api_macros() -> Result<(), Error> {
    let mut eg = fib()?;
    datatype!(&mut eg, (datatype Expr (One) (Two Expr Expr :cost 10)));
    add_ruleset(&mut eg, "custom")?;
    rule(
        &mut eg,
        "custom",
        facts![(fib 5) (fib x) (= f1 (fib (+ x 1))) (= 3 (unquote exprs::int(1 + 2)))],
        actions![
            (let y (+ x 2))
            (set (fib (+ x 2)) (+ f1 f1))
            (delete (fib 0))
            (subsume (Two (One) (One)))
            (union (One) (Two (One) (One)))
            (panic "message")
            (+ 6 87)
        ],
    )?;
    assert_eq!(eg.get_function("Two").and_then(|f| f.cost()), Some(10));
    Ok(())
}

fn call(head: &str, args: Vec<Expr>) -> Expr {
    exprs::call(head, args)
}

fn big(digits: &str) -> Expr {
    call("from-string", vec![exprs::string(digits)])
}

fn shown(eg: &mut EGraph, expr: Expr) -> String {
    let value = eg.eval_expr(&call("to-string", vec![expr])).unwrap().1;
    eg.value_to_base(value)
}

fn rat(num: i64, den: i64) -> Expr {
    call(
        "bigrat",
        vec![
            call("bigint", vec![exprs::int(num)]),
            call("bigint", vec![exprs::int(den)]),
        ],
    )
}

// The numerator and denominator of a BigRat expression.
fn parts(eg: &mut EGraph, expr: Expr) -> (String, String) {
    let numer = shown(eg, call("numer", vec![expr.clone()]));
    (numer, shown(eg, call("denom", vec![expr])))
}

fn defined(eg: &mut EGraph, expr: Expr) -> bool {
    eg.eval_expr(&expr).is_ok()
}

#[test]
fn bigint_primitives() {
    let mut eg = EGraph::default();
    let product = call(
        "*",
        vec![
            big("123456789012345678901234567890"),
            big("987654321098765432109876543210"),
        ],
    );
    assert_eq!(
        shown(&mut eg, product),
        "121932631137021795226185032733622923332237463801111263526900"
    );
    let shifted = call(
        "<<",
        vec![call("bigint", vec![exprs::int(1)]), exprs::int(100)],
    );
    assert_eq!(
        shown(&mut eg, shifted),
        "1267650600228229401496703205376",
        "2^100"
    );
    let z = |n: i64| call("bigint", vec![exprs::int(n)]);
    assert_eq!(
        shown(&mut eg, call("/", vec![z(-7), z(2)])),
        "-3",
        "division truncates"
    );
    assert_eq!(shown(&mut eg, call("%", vec![z(-7), z(2)])), "-1");
    assert_eq!(
        shown(&mut eg, call(">>", vec![z(-5), exprs::int(1)])),
        "-3",
        "a shift floors"
    );
    assert_eq!(shown(&mut eg, call("&", vec![z(-1), z(255)])), "255");
    assert_eq!(shown(&mut eg, call("|", vec![z(-256), z(255)])), "-1");
    assert_eq!(shown(&mut eg, call("not-Z", vec![z(5)])), "-6");
    assert_eq!(shown(&mut eg, call("bits", vec![z(255)])), "8");
    assert_eq!(
        shown(
            &mut eg,
            call("-", vec![big("-100000000000000000000"), big("1")])
        ),
        "-100000000000000000001"
    );
    assert!(!defined(&mut eg, call("/", vec![z(1), z(0)])));
    assert!(defined(
        &mut eg,
        call("<", vec![big("-100000000000000000000"), z(-1)])
    ));
    let same = call("+", vec![big("99999999999999999999"), z(1)]);
    assert_eq!(
        eg.eval_expr(&same).unwrap().1,
        eg.eval_expr(&big("100000000000000000000")).unwrap().1,
        "equal integers are one value"
    );
}

#[test]
fn bigrat_primitives() {
    let mut eg = EGraph::default();
    assert_eq!(
        parts(&mut eg, call("sqrt", vec![rat(0, 1)])),
        ("0".to_owned(), "1".to_owned())
    );
    assert_eq!(
        parts(&mut eg, rat(6, -4)),
        ("-3".to_owned(), "2".to_owned()),
        "lowest terms, positive denominator"
    );
    assert_eq!(
        parts(&mut eg, call("+", vec![rat(1, 2), rat(1, 3)])),
        ("5".to_owned(), "6".to_owned())
    );
    assert_eq!(parts(&mut eg, call("floor", vec![rat(-3, 2)])).0, "-2");
    assert_eq!(parts(&mut eg, call("ceil", vec![rat(-3, 2)])).0, "-1");
    assert_eq!(
        parts(&mut eg, call("round", vec![rat(-3, 2)])).0,
        "-2",
        "halves round away from zero"
    );
    assert_eq!(parts(&mut eg, call("round", vec![rat(5, 2)])).0, "3");
    assert_eq!(
        parts(&mut eg, call("pow", vec![rat(2, 3), rat(-2, 1)])),
        ("9".to_owned(), "4".to_owned())
    );
    assert_eq!(
        parts(&mut eg, call("sqrt", vec![rat(9, 4)])),
        ("3".to_owned(), "2".to_owned())
    );
    assert!(!defined(&mut eg, call("sqrt", vec![rat(2, 1)])));
    assert!(!defined(&mut eg, rat(1, 0)));
    let quarter = eg.eval_expr(&call("to-f64", vec![rat(1, 4)])).unwrap().1;
    assert_eq!(eg.value_to_base::<tir_egglog::F64>(quarter).0, 0.25);
    let two = eg.eval_expr(&call("to-i64", vec![rat(4, 2)])).unwrap().1;
    assert_eq!(eg.value_to_base::<i64>(two), 2);
}

#[test]
fn base_primitives() {
    let mut eg = EGraph::default();
    let int = |eg: &mut EGraph, expr| {
        let v = eg.eval_expr(&expr).unwrap().1;
        eg.value_to_base::<i64>(v)
    };
    assert_eq!(
        int(&mut eg, call("<<", vec![exprs::int(1), exprs::int(3)])),
        8
    );
    assert_eq!(int(&mut eg, call("log2", vec![exprs::int(8)])), 3);
    assert_eq!(
        int(
            &mut eg,
            call(
                "count-matches",
                vec![exprs::string("banana"), exprs::string("an")]
            )
        ),
        2
    );
    assert_eq!(
        int(
            &mut eg,
            call("ordering-max", vec![exprs::int(3), exprs::int(5)])
        ),
        5
    );
    assert!(!defined(
        &mut eg,
        call("/", vec![exprs::int(7), exprs::int(0)])
    ));
    assert_eq!(shown(&mut eg, exprs::int(42)), "42");
    assert_eq!(shown(&mut eg, exprs::float(1.5)), "1.5");
    assert!(!defined(
        &mut eg,
        call("/", vec![exprs::float(1.0), exprs::float(0.0)])
    ));
    assert!(!defined(&mut eg, call("sqrt", vec![exprs::float(-1.0)])));
    let text = |eg: &mut EGraph, expr| {
        let v = eg.eval_expr(&expr).unwrap().1;
        eg.value_to_base::<String>(v)
    };
    let words = vec![exprs::string("a"), exprs::string("b"), exprs::string("c")];
    assert_eq!(text(&mut eg, call("+", words)), "abc");
    let replaced = call(
        "replace",
        vec![
            exprs::string("aXbX"),
            exprs::string("X"),
            exprs::string("-"),
        ],
    );
    assert_eq!(text(&mut eg, replaced), "a-b-");
    let truth = |eg: &mut EGraph, expr| {
        let v = eg.eval_expr(&expr).unwrap().1;
        eg.value_to_base::<bool>(v)
    };
    assert!(truth(
        &mut eg,
        call(
            "or",
            vec![exprs::bool(false), exprs::bool(false), exprs::bool(true)]
        )
    ));
    assert!(!truth(
        &mut eg,
        call("=>", vec![exprs::bool(true), exprs::bool(false)])
    ));
    assert!(!defined(&mut eg, call("guard", vec![exprs::bool(false)])));
    assert!(defined(
        &mut eg,
        call("!=", vec![exprs::int(1), exprs::int(2)])
    ));
    assert!(!defined(
        &mut eg,
        call("!=", vec![exprs::int(1), exprs::int(1)])
    ));
}

#[test]
fn print_function_and_print_size() -> Result<(), Error> {
    let mut eg = with_function();
    run(
        &mut eg,
        vec![action!((set (f 1) 42)), action!((set (f 2) 43))],
    )?;
    let print = |mode| Command::PrintFunction(span!(), "f".to_owned(), None, None, mode);
    let outputs = eg.run_program(vec![
        print(tir_egglog::ast::PrintFunctionMode::Default),
        print(tir_egglog::ast::PrintFunctionMode::CSV),
        Command::PrintSize(span!(), Some("f".to_owned())),
        Command::PrintSize(span!(), None),
    ])?;
    let text: Vec<String> = outputs.iter().map(ToString::to_string).collect();
    assert_eq!(
        text,
        [
            "(\n   (f 1) -> 42\n   (f 2) -> 43\n)\n",
            "f,1,42\nf,2,43\n",
            "2\n",
            "((f 2))\n"
        ]
    );
    Ok(())
}

struct Echo;

impl tir_egglog::UserDefinedCommand for Echo {
    fn update(
        &self,
        _: &mut EGraph,
        args: &[Expr],
    ) -> Result<Vec<tir_egglog::CommandOutput>, Error> {
        let output = format!("{} args", args.len());
        Ok(vec![tir_egglog::CommandOutput::UserDefined(Arc::new(
            output,
        ))])
    }
}

#[test]
fn user_defined_commands_run() -> Result<(), Error> {
    let mut eg = EGraph::default();
    eg.add_command("echo".to_owned(), Arc::new(Echo))?;
    assert!(eg.has_command("echo"));
    assert!(matches!(
        eg.add_command("echo".to_owned(), Arc::new(Echo)),
        Err(Error::CommandAlreadyExists(..))
    ));
    let outputs = eg.run_program(vec![Command::UserDefined(
        span!(),
        "echo".to_owned(),
        vec![exprs::int(1), exprs::int(2)],
    )])?;
    assert_eq!(outputs[0].to_string(), "2 args");
    Ok(())
}

// The constant folding of `const_fold` with its callback, saved offline and
// loaded into a fresh e-graph that binds the callback again.
#[test]
fn a_saved_program_with_a_rust_rule_loads_and_runs() -> Result<(), Error> {
    let fold = |mut ctx: tir_egglog::WriteState<'_, '_>, vals: &[Value]| {
        let [sum, _, _, a, b] = vals else {
            unreachable!()
        };
        let (a, b) = (ctx.value_to_base::<i64>(*a), ctx.value_to_base::<i64>(*b));
        let folded = ctx.add("Num", a + b).ok()?;
        ctx.union(*sum, folded).ok()
    };
    let bytes = const_fold()?.save_program();
    let mut online = EGraph::default();
    online.bind_rust_rule("fold_add", fold);
    online.load_program(&bytes)?;
    let root = online
        .eval_expr(&expr!((Add (Num 2) (Add (Num 3) (Num 4)))))?
        .1;
    for _ in 0..3 {
        online.step_rules("const_fold")?;
    }
    let nine = online.eval_expr(&expr!((Num 9)))?.1;
    assert_eq!(online.database().find(root), online.database().find(nine));
    let added = rule(
        &mut online,
        "const_fold",
        facts![(Num x)],
        actions![(Num (+ x 1))],
    );
    assert!(
        matches!(added, Err(Error::Unsupported(_))),
        "a loaded program takes no new rules"
    );
    Ok(())
}

#[test]
fn a_combined_ruleset_runs_its_members_in_one_iteration() -> Result<(), Error> {
    let mut eg = EGraph::default();
    for name in ["R", "S", "T"] {
        add_relation(&mut eg, name, vec!["i64".into()])?;
    }
    add_ruleset(&mut eg, "a")?;
    add_ruleset(&mut eg, "b")?;
    rule(&mut eg, "a", facts![(R x)], actions![(S x)])?;
    rule(&mut eg, "b", facts![(S x)], actions![(T x)])?;
    eg.run_program(vec![Command::UnstableCombinedRuleset(
        span!(),
        "c".to_owned(),
        vec!["a".into(), "b".into()],
    )])?;
    run(&mut eg, vec![action!((R 1))])?;
    eg.step_rules("c")?;
    assert_eq!(
        (eg.get_size("S"), eg.get_size("T")),
        (1, 0),
        "b matches before a writes"
    );
    eg.step_rules("c")?;
    assert_eq!(eg.get_size("T"), 1);
    assert!(!eg.step_rules("a")?.updated, "a ran in c, so R 1 is old");
    let added = rule(&mut eg, "c", facts![(R x)], actions![(T x)]);
    assert!(matches!(added, Err(Error::CombinedRulesetError(..))));
    Ok(())
}

#[test]
fn check_and_fail_commands() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_relation(&mut eg, "R", vec!["i64".into()])?;
    run(&mut eg, vec![action!((R 1))])?;
    let check = |n: i64| Command::Check(span!(), facts![(R (unquote exprs::int(n)))].0);
    eg.run_program(vec![check(1)])?;
    assert!(matches!(
        eg.run_program(vec![check(99)]),
        Err(Error::CheckError(..))
    ));
    eg.run_program(vec![Command::Fail(span!(), Box::new(check(99)))])?;
    assert!(matches!(
        eg.run_program(vec![Command::Fail(span!(), Box::new(check(1)))]),
        Err(Error::ExpectFail(_))
    ));
    Ok(())
}

// A macro that declares a copy of each relation.
struct Copies;

impl tir_egglog::CommandMacro for Copies {
    fn transform(&self, command: Command, _: &tir_egglog::Database) -> Result<Vec<Command>, Error> {
        let Command::Relation { span, name, inputs } = &command else {
            return Ok(vec![command]);
        };
        let copy = Command::Relation {
            span: *span,
            name: format!("{name}_copy"),
            inputs: inputs.clone(),
        };
        Ok(vec![command, copy])
    }
}

#[test]
fn command_macros_and_relaxed_rulesets() -> Result<(), Error> {
    let mut eg = EGraph::default();
    eg.command_macros_mut().register(Arc::new(Copies));
    add_relation(&mut eg, "R", vec!["i64".into()])?;
    assert!(eg.get_function("R_copy").is_some());
    datatype!(&mut eg, (datatype Math (Num i64) (Add Math Math)));
    eg.set_ruleset_mode("", tir_egglog::Mode::Relaxed);
    let rewrite = tir_egglog::ast::Rewrite {
        span: span!(),
        lhs: expr!((Add a b)),
        rhs: expr!((Add b a)),
        conditions: Vec::new(),
        name: String::new(),
    };
    eg.run_program(vec![Command::Rewrite(String::new(), rewrite, false)])?;
    run(&mut eg, vec![action!((Add (Num 1) (Num 2)))])?;
    run_ruleset(&mut eg, "")?;
    let (ab, ba) = (
        eg.eval_expr(&expr!((Add (Num 1) (Num 2))))?.1,
        eg.eval_expr(&expr!((Add (Num 2) (Num 1))))?.1,
    );
    assert_eq!(eg.database().find(ab), eg.database().find(ba));
    Ok(())
}

// egglog's tests/integration_test.rs: a query variable may not reuse the
// name of a function or the name of a global without its `$`.
#[test]
fn variables_may_not_shadow_declarations() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_function(&mut eg, "f", schema(&[], "i64"), None)?;
    run(&mut eg, vec![action!((set (f) 2))])?;
    let check = Command::Check(span!(), facts![(= (f) f) (= f 2)].0);
    assert!(matches!(
        eg.run_program(vec![check]),
        Err(Error::Shadowing(..))
    ));

    let mut eg = EGraph::default();
    run(&mut eg, vec![global("$value", exprs::int(41))])?;
    let shadow = Rule {
        span: span!(),
        head: actions![],
        body: Facts(vec![tir_egglog::ast::Fact::Eq(
            span!(),
            exprs::var("value"),
            exprs::var("$value"),
        )])
        .0,
        name: String::new(),
        ruleset: String::new(),
        eval_mode: RuleEvalMode::Seminaive,
    };
    let Err(Error::Shadowing(message, ..)) = eg.run_program(vec![Command::Rule { rule: shadow }])
    else {
        panic!("a shadowing error")
    };
    assert!(message.contains("pattern variable `value`") && message.contains("global `$value`"));
    Ok(())
}

#[test]
fn subsuming_a_function_row_is_an_error() -> Result<(), Error> {
    let mut eg = with_function();
    run(&mut eg, vec![action!((set (f 1) 1))])?;
    let added = rule(&mut eg, "", facts![(= y (f x))], actions![(subsume (f x))]);
    assert!(matches!(added, Err(Error::SubsumeMergeError(name, _)) if name == "f"));
    assert!(matches!(
        run(&mut eg, vec![action!((subsume (f 1)))]),
        Err(Error::SubsumeMergeError(..))
    ));
    let mut eg = with_list()?;
    run(&mut eg, vec![action!((subsume(Nil)))])?;
    Ok(())
}
