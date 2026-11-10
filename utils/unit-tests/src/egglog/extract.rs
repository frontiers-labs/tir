//! Extraction with `:cost` and `:unextractable`, variants and a custom cost
//! model; extraction attributes and defaults across a program file; and
//! schedulers (egglog's src/scheduler.rs tests).

use std::sync::Arc;

use tir_egglog::ast::{Command, Schema};
use tir_egglog::extract::{DagCostModel, GreedyDagExtractor, TreeCostModel};
use tir_egglog::prelude::*;
use tir_egglog::scheduler::{Matches, Scheduler, SchedulerContext};
use tir_egglog::{
    ArcSort, CommandOutput, Database, Enode, Error, Merge, Program, Rules, TableSchema, Value,
};

fn schema(input: &[&str], output: &str) -> Schema {
    Schema::new(
        input.iter().map(|&s| s.to_owned()).collect(),
        output.to_owned(),
    )
}

fn run(eg: &mut EGraph, actions: Vec<Action>) {
    eg.run_program(actions.into_iter().map(Command::Action).collect())
        .unwrap();
}

// The best term of `expr` and its cost.
fn best(eg: &mut EGraph, expr: Expr) -> Result<(String, u64), Error> {
    let (sort, value) = eg.eval_expr(&expr)?;
    let (termdag, term, cost) = eg.extract_value(&sort, value)?;
    Ok((termdag.to_string(term), cost))
}

#[test]
fn cost_picks_the_cheaper_constructor() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_sort(&mut eg, "Math")?;
    add_constructor(&mut eg, "Cheap", schema(&["i64"], "Math"), Some(1), false)?;
    add_constructor(&mut eg, "Dear", schema(&["i64"], "Math"), Some(100), false)?;
    run(&mut eg, vec![action!((union (Cheap 1) (Dear 1)))]);
    assert_eq!(
        best(&mut eg, expr!((Dear 1)))?,
        ("(Cheap 1)".to_owned(), 2),
        "1 for Cheap, 1 for the literal"
    );
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype Math (Cheap i64 :cost 50) (Dear i64 :cost 7)));
    run(&mut eg, vec![action!((union (Cheap 1) (Dear 1)))]);
    assert_eq!(best(&mut eg, expr!((Cheap 1)))?, ("(Dear 1)".to_owned(), 8));
    Ok(())
}

#[test]
fn colored_extraction_costs_normalized_containers() -> Result<(), Error> {
    use tir_egglog::sort::{Container, MapKind, MultiSetKind, SetKind, VecKind};
    use tir_egglog::{Color, Extractor, SortKind, TermDag};

    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype E (A :cost 2) (B :cost 3)));
    add_constructor(&mut eg, "Hidden", schema(&[], "E"), None, true)?;
    for (name, presort, args) in [
        ("ES", "Set", vec!["E"]),
        ("EV", "Vec", vec!["E"]),
        ("EMS", "MultiSet", vec!["E"]),
        ("EM", "Map", vec!["E", "i64"]),
        ("HiddenMap", "Map", vec!["E", "E"]),
        ("Nested", "Set", vec!["ES"]),
    ] {
        eg.run_program(vec![Command::Sort {
            span: span!(),
            name: name.to_owned(),
            presort_and_args: Some((
                presort.to_owned(),
                args.into_iter().map(exprs::var).collect(),
            )),
        }])?;
    }
    let a = eg.eval_expr(&expr!((A)))?.1;
    let b = eg.eval_expr(&expr!((B)))?.1;
    let hidden = eg.eval_expr(&expr!((Hidden)))?.1;
    let db = eg.database_mut();
    let set = db.container(Container::<SetKind>::new(vec![a, b], [true; 2]));
    let sa = db.container(Container::<SetKind>::new(vec![a], [true; 2]));
    let sb = db.container(Container::<SetKind>::new(vec![b], [true; 2]));
    let nested = db.container(Container::<SetKind>::new(vec![sa, sb], [true; 2]));
    let vec = db.container(Container::<VecKind>::new(vec![a, b], [true; 2]));
    let multiset = db.container(Container::<MultiSetKind>::new(vec![a, b], [true; 2]));
    let seven = db.base(7_i64);
    let nine = db.base(9_i64);
    let map = db.container(Container::<MapKind>::new(
        vec![a, seven, b, nine],
        [true, false],
    ));
    let hidden_map = db.container(Container::<MapKind>::new(vec![a, hidden, b, a], [true; 2]));
    let root_sort = db.add_sort("Root", SortKind::Eq);
    let chosen = db.add_constructor("Chosen", &[db.sort("ES")?], root_sort);
    let fallback = db.add_constructor("Fallback", &[], root_sort);
    db.set_cost(fallback, 4);
    let root = db.fresh(root_sort);
    db.insert(chosen, &[set, root])?;
    db.insert(fallback, &[root])?;
    let hidden_root = db.fresh(root_sort);
    let only = db.add_constructor("Only", &[db.sort("HiddenMap")?], root_sort);
    db.insert(only, &[hidden_map, hidden_root])?;
    let parent = db.color(&[]);
    let child = db.color(&[parent]);
    let sibling = db.color(&[]);
    db.union_in(parent, a, b);
    db.rebuild()?;
    for color in [parent, child, Color::BASE, sibling] {
        let local = color == parent || color == child;
        let extractor = Extractor::new_in(db, color);
        let mut terms = TermDag::default();
        for (sort, value, local_term, local_cost, base_term, base_cost) in [
            ("ES", set, "(set-of (A))", 2, "(set-of (A) (B))", 5),
            ("EV", vec, "(vec-of (A) (A))", 4, "(vec-of (A) (B))", 5),
            (
                "EMS",
                multiset,
                "(multiset-of (A) (A))",
                4,
                "(multiset-of (A) (B))",
                5,
            ),
            ("EM", map, "(map-of (A) 9)", 3, "(map-of (A) 7 (B) 9)", 7),
            (
                "Nested",
                nested,
                "(set-of (set-of (A)))",
                2,
                "(set-of (set-of (A)) (set-of (B)))",
                5,
            ),
        ] {
            let sort = db.schema().sort(db.sort(sort)?).arcsort().clone();
            let dag_extractor = GreedyDagExtractor::with_cost_model_in(
                db,
                color,
                &[(sort.clone(), value)],
                tir_egglog::extract::AdditiveCostModel::default(),
            )?;
            let dag_extracted = dag_extractor
                .extract_best_with_sort(&mut terms, value, sort.clone())
                .unwrap();
            let extracted = extractor
                .extract_best_with_sort(&mut terms, value, sort)
                .unwrap();
            let expected = if local {
                (local_term, local_cost)
            } else {
                (base_term, base_cost)
            };
            assert_eq!(
                (terms.to_string(extracted.term), extracted.cost),
                (expected.0.to_owned(), expected.1),
                "{color:?}"
            );
            let dag_cost =
                if local && matches!(expected.0, "(vec-of (A) (A))" | "(multiset-of (A) (A))") {
                    2
                } else {
                    expected.1
                };
            assert_eq!(
                (terms.to_string(dag_extracted.term), dag_extracted.cost),
                (expected.0.to_owned(), dag_cost),
                "DAG {color:?}"
            );
        }
        let sort = db.schema().sort(root_sort).arcsort().clone();
        let dag_extractor = GreedyDagExtractor::with_cost_model_in(
            db,
            color,
            &[(sort.clone(), root), (sort.clone(), hidden_root)],
            tir_egglog::extract::AdditiveCostModel::default(),
        )?;
        let dag_root = dag_extractor
            .extract_best_with_sort(&mut terms, root, sort.clone())
            .unwrap();
        let extracted = extractor
            .extract_best_with_sort(&mut terms, root, sort.clone())
            .unwrap();
        let expected = if local {
            ("(Chosen (set-of (A)))", 3)
        } else {
            ("(Fallback)", 4)
        };
        assert_eq!(
            (terms.to_string(extracted.term), extracted.cost),
            (expected.0.to_owned(), expected.1)
        );
        assert_eq!(
            (terms.to_string(dag_root.term), dag_root.cost),
            (expected.0.to_owned(), expected.1)
        );
        let dag_hidden =
            dag_extractor.extract_best_with_sort(&mut terms, hidden_root, sort.clone());
        let extracted = extractor.extract_best_with_sort(&mut terms, hidden_root, sort);
        if local {
            let extracted = extracted.unwrap();
            assert_eq!(
                (terms.to_string(extracted.term), extracted.cost),
                ("(Only (map-of (A) (A)))".to_owned(), 5)
            );
            let extracted = dag_hidden.unwrap();
            assert_eq!(
                (terms.to_string(extracted.term), extracted.cost),
                ("(Only (map-of (A) (A)))".to_owned(), 3)
            );
        } else {
            assert!(extracted.is_none());
            assert!(dag_hidden.is_none());
        }
    }
    Ok(())
}

#[test]
fn unextractable_constructors_are_skipped() -> Result<(), Error> {
    let mut eg = EGraph::default();
    add_sort(&mut eg, "Math")?;
    add_constructor(&mut eg, "Num", schema(&["i64"], "Math"), Some(5), false)?;
    add_constructor(&mut eg, "Hidden", schema(&["i64"], "Math"), Some(0), true)?;
    add_constructor(&mut eg, "Only", schema(&["i64"], "Math"), None, true)?;
    run(
        &mut eg,
        vec![action!((union (Hidden 1) (Num 1))), action!((Only 2))],
    );
    assert_eq!(best(&mut eg, expr!((Hidden 1)))?.0, "(Num 1)");
    assert!(matches!(
        best(&mut eg, expr!((Only 2))),
        Err(Error::ExtractError(_))
    ));
    Ok(())
}

fn three() -> Result<EGraph, Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype Math (Num i64) (Add Math Math)));
    run(
        &mut eg,
        vec![action!((union (Num 3) (Add (Num 1) (Num 2))))],
    );
    Ok(eg)
}

#[test]
fn variants_are_the_cheapest_nodes_of_the_class() -> Result<(), Error> {
    let mut eg = three()?;
    let (sort, value) = eg.eval_expr(&expr!((Num 3)))?;
    let extracted = eg.extract_variants(vec![(sort, value)], 5)?;
    let variants: Vec<(String, u64)> = extracted.variants[0]
        .iter()
        .map(|v| (extracted.termdag.to_string(v.term), v.cost))
        .collect();
    let expected = [
        ("(Num 3)".to_owned(), 2),
        ("(Add (Num 1) (Num 2))".to_owned(), 5),
    ];
    assert_eq!(variants, expected);
    let outputs = eg.run_program(vec![Command::Extract(
        span!(),
        expr!((Num 3)),
        exprs::int(1),
    )])?;
    let [CommandOutput::ExtractVariants(termdag, terms)] = &outputs[..] else {
        panic!("one variants output")
    };
    let terms: Vec<String> = terms.iter().map(|&t| termdag.to_string(t)).collect();
    assert_eq!(terms, ["(Num 3)"]);
    Ok(())
}

// A literal costs its square; a node 1 and its children.
struct Squares;

impl TreeCostModel<u64> for Squares {
    type EnodeCost = ();
    type ContainerCost = ();

    fn base_value_cost(&self, db: &Database, _: &ArcSort, value: Value) -> u64 {
        db.base_value::<i64>(value).unsigned_abs().pow(2)
    }

    fn enode_cost(&self, _: &Database, _: &TableSchema, _: &Enode<'_>) {}

    fn container_cost(&self, _: &Database, _: &ArcSort, _: Value) {}

    fn fold_enode_cost(&self, (): (), children: &[u64]) -> u64 {
        1 + children.iter().sum::<u64>()
    }

    fn fold_container_cost(&self, (): (), elements: &[u64]) -> u64 {
        elements.iter().sum()
    }
}

#[test]
fn a_custom_cost_model_decides() -> Result<(), Error> {
    let mut eg = three()?;
    let (sort, value) = eg.eval_expr(&expr!((Num 3)))?;
    let extracted = eg.extract_best_with_cost_model(vec![(sort.clone(), value)], Squares)?;
    let root = extracted.terms[0].as_ref().unwrap();
    assert_eq!(
        extracted.termdag.to_string(root.term),
        "(Add (Num 1) (Num 2))"
    );
    assert_eq!(
        root.cost,
        1 + (1 + 1) + (1 + 4),
        "against 1 + 9 for (Num 3)"
    );
    let default = eg.extract_best(vec![(sort, value)])?;
    let root = default.terms[0].as_ref().unwrap();
    assert_eq!(default.termdag.to_string(root.term), "(Num 3)");
    Ok(())
}

// Cost, extractability and a function default go into the program file,
// so a load into a database that lacks the tables declares them so.
#[test]
fn extraction_attributes_and_defaults_survive_a_program_file() -> Result<(), Error> {
    let mut db = Database::new();
    let (int, math) = (
        db.sort("i64")?,
        db.add_sort("Math", tir_egglog::SortKind::Eq),
    );
    db.add_relation("item", &[int]);
    db.add_relation("seen", &[int]);
    let num = db.add_constructor("Num", &[int], math);
    let hidden = db.add_constructor("Hidden", &[int], math);
    let f = db.add_function("f", &[int], int, Merge::AssertEq);
    let seven = db.base(7_i64);
    db.set_cost(num, 3);
    db.set_unextractable(hidden, true);
    db.set_default(f, seven);
    let mut rules = Rules::new();
    rules.rule(
        &db,
        "",
        facts![(item x)],
        actions![(seen (f x)) (union (Num x) (Hidden x))],
    )?;
    let bytes = rules.compile(&db)?.to_bytes();

    let mut fresh = Database::new();
    let mut program = Program::from_bytes(&bytes, &mut fresh).unwrap();
    let schema = |db: &Database, name: &str| db.schema().table(db.table(name).unwrap()).clone();
    assert_eq!(schema(&fresh, "Num").cost(), Some(3));
    assert!(schema(&fresh, "Hidden").is_unextractable());
    assert_eq!(schema(&fresh, "f").default_value(), Some(seven));
    let one = fresh.base(1_i64);
    fresh.insert(fresh.table("item")?, &[one])?;
    program.run(&mut fresh, &tir_egglog::Schedule::run(""), &[])?;
    let rows: Vec<Value> = fresh.rows(fresh.table("seen")?).map(|row| row[0]).collect();
    assert_eq!(
        rows,
        [seven],
        "a lookup of a missing key writes the default"
    );
    let class = fresh.lookup(fresh.table("Hidden")?, &[one]).unwrap();
    let (termdag, term) = tir_egglog::Extractor::new(&fresh).best(class)?;
    assert_eq!(termdag.to_string(term), "(Num 1)");
    Ok(())
}

#[derive(Clone)]
struct FirstN(usize);

impl Scheduler for FirstN {
    fn filter_matches(
        &mut self,
        _: &SchedulerContext<'_>,
        _: &str,
        _: &str,
        matches: &mut Matches,
    ) -> bool {
        if matches.match_size() <= self.0 {
            matches.choose_all();
        } else {
            (0..self.0).for_each(|i| matches.choose(i));
        }
        matches.match_size() < self.0 * 2
    }
}

// R holds 0 to 100; each step of `test` copies ten more rows of R into S,
// and the twelfth step finds nothing left.
#[test]
fn first_n_scheduler_takes_ten_matches_a_step() -> Result<(), Error> {
    let mut eg = EGraph::default();
    eg.set_report_level(tir_egglog::ReportLevel::WithPlan);
    let id = eg.add_scheduler(Box::new(FirstN(10)));
    add_relation(&mut eg, "R", vec!["i64".into()])?;
    run(&mut eg, vec![action!((R 0))]);
    rule(&mut eg, "", facts![(R x) (< x 100)], actions![(R (+ x 1))])?;
    let saturate = tir_egglog::Schedule::Saturate(span!(), Box::new(tir_egglog::Schedule::run("")));
    eg.run_program(vec![Command::RunSchedule(saturate)])?;
    assert_eq!(eg.get_size("R"), 101);
    add_ruleset(&mut eg, "test")?;
    add_relation(&mut eg, "S", vec!["i64".into()])?;
    let copy = tir_egglog::ast::Rule {
        span: span!(),
        head: actions![(S x)],
        body: facts![(R x)].0,
        name: "test-rule".to_owned(),
        ruleset: "test".to_owned(),
        eval_mode: tir_egglog::ast::RuleEvalMode::Seminaive,
    };
    eg.run_program(vec![Command::Rule { rule: copy }])?;
    let mut steps = 0;
    loop {
        let report = eg.step_rules_with_scheduler(id, "test")?;
        steps += 1;
        assert_eq!(eg.get_size("S"), (steps * 10).min(101));
        let expected = if steps <= 10 { 10 } else { 12 - steps };
        let name: Arc<str> = "test-rule".into();
        let matches: Vec<_> = report.num_matches_per_rule.iter().collect();
        match expected {
            0 => assert!(matches.is_empty()),
            n => assert_eq!(matches, [(&name, &n)]),
        }
        if report.can_stop {
            break;
        }
    }
    assert_eq!(steps, 12);
    Ok(())
}

#[derive(Clone)]
struct DrainThree;

impl Scheduler for DrainThree {
    fn filter_matches(
        &mut self,
        _: &SchedulerContext<'_>,
        _: &str,
        _: &str,
        matches: &mut Matches,
    ) -> bool {
        (0..matches.match_size().min(3)).for_each(|i| matches.choose(i));
        false
    }
}

#[test]
fn a_naive_scheduler_keeps_the_matches_it_held_back() -> Result<(), Error> {
    let mut eg = EGraph::default();
    eg.seminaive = false;
    let id = eg.add_scheduler(Box::new(DrainThree));
    add_ruleset(&mut eg, "test")?;
    add_relation(&mut eg, "R", vec!["i64".into()])?;
    add_relation(&mut eg, "S", vec!["i64".into()])?;
    rule(&mut eg, "test", facts![(R x)], actions![(S x)])?;
    run(
        &mut eg,
        (0..20)
            .map(|i| Action::Expr(span!(), exprs::call("R", vec![exprs::int(i)])))
            .collect(),
    );
    for expected in [3, 6, 9, 12, 15, 18, 20] {
        eg.step_rules_with_scheduler(id, "test")?;
        assert_eq!(eg.get_size("S"), expected);
    }
    assert!(eg.remove_scheduler(id).is_some());
    assert!(eg.step_rules_with_scheduler(id, "test").is_err());
    Ok(())
}

// The node count a scheduler sees while it filters each rule.
#[derive(Clone, Default)]
struct Watch(Arc<std::sync::Mutex<Vec<(String, usize)>>>);

impl Scheduler for Watch {
    fn filter_matches(
        &mut self,
        ctx: &SchedulerContext<'_>,
        rule: &str,
        _: &str,
        matches: &mut Matches,
    ) -> bool {
        self.0
            .lock()
            .unwrap()
            .push((rule.to_owned(), ctx.num_nodes()));
        matches.choose_all();
        true
    }
}

// As upstream, the chosen actions of each rule apply before the next rule is
// filtered, and one rebuild ends the iteration: the second rule still sees
// both F rows that the union of the first rule makes congruent, and only the
// end of the iteration sees them merged.
#[test]
fn a_scheduler_iteration_rebuilds_once_after_every_rule() -> Result<(), Error> {
    let mut eg = EGraph::default();
    let watch = Watch::default();
    let id = eg.add_scheduler(Box::new(watch.clone()));
    datatype!(&mut eg, (datatype E (Leaf i64) (F E)));
    run(&mut eg, vec![action!((F (Leaf 1))), action!((F (Leaf 2)))]);
    add_ruleset(&mut eg, "test")?;
    let rule = |name: &str, body: Facts, head| tir_egglog::ast::Rule {
        span: span!(),
        head,
        body: body.0,
        name: name.to_owned(),
        ruleset: "test".to_owned(),
        eval_mode: tir_egglog::ast::RuleEvalMode::Seminaive,
    };
    let unite = rule(
        "unite",
        facts![(= x (Leaf 1)) (= y (Leaf 2))],
        actions![(union x y)],
    );
    let look = rule("look", facts![(Leaf 1)], actions![(Leaf 1)]);
    eg.run_program(vec![
        Command::Rule { rule: unite },
        Command::Rule { rule: look },
    ])?;
    eg.step_rules_with_scheduler(id, "test")?;
    let seen = watch.0.lock().unwrap().clone();
    let seen: Vec<(&str, usize)> = seen.iter().map(|(rule, n)| (rule.as_str(), *n)).collect();
    assert_eq!(seen, [("unite", 4), ("look", 4)]);
    assert_eq!(eg.num_nodes(), 3);
    Ok(())
}

#[test]
fn equal_cost_parent_keeps_an_acyclic_derivation() {
    let mut db = Database::new();
    let node = db.add_sort("Node", tir_egglog::SortKind::Eq);
    let expensive = db.add_constructor("Expensive", &[], node);
    let wrapper = db.add_constructor("Wrapper", &[node], node);
    let cheap = db.add_constructor("Cheap", &[], node);
    db.set_cost(expensive, 20);
    db.set_cost(wrapper, u64::MAX);
    db.set_cost(cheap, 1);
    let x = db.entry(expensive, &[]).unwrap();
    let root = db.entry(wrapper, &[x]).unwrap();
    let y = db.entry(cheap, &[]).unwrap();
    db.union(x, y);
    db.rebuild().unwrap();
    let (dag, term) = tir_egglog::Extractor::new(&db).best(root).unwrap();
    assert_eq!(dag.to_string(term), "(Wrapper (Cheap))");
}

#[test]
fn scheduler_retains_selected_match_occurrences() -> Result<(), Error> {
    for capture in [false, true] {
        let mut eg = EGraph::default();
        add_relation(&mut eg, "R", vec!["i64".into(), "i64".into()])?;
        add_relation(&mut eg, "Seen", vec!["i64".into()])?;
        add_function(
            &mut eg,
            "count",
            tir_egglog::ast::Schema::new(vec![], "i64".into()),
            Some(expr!((+ old new))),
        )?;
        add_ruleset(&mut eg, "C")?;
        let head = if capture {
            actions![(Seen y) (set (count) 1)]
        } else {
            actions![(set (count) 1)]
        };
        rule(&mut eg, "C", facts![(R x y)], head)?;
        run(
            &mut eg,
            vec![action!((R 1 7)), action!((R 2 7)), action!((set (count) 0))],
        );
        let id = eg.add_scheduler(Box::new(DrainThree));
        eg.step_rules_with_scheduler(id, "C")?;
        let (_, count) = eg.eval_expr(&expr!((count)))?;
        assert_eq!(eg.value_to_base::<i64>(count), 2);
    }
    Ok(())
}

#[test]
fn scheduler_records_belong_to_the_declaring_ruleset() -> Result<(), Error> {
    let mut eg = EGraph::default();
    for name in ["R", "S", "OutA", "OutB"] {
        add_relation(&mut eg, name, vec!["i64".into()])?;
    }
    for (set, body, head) in [
        ("A", facts![(R x)], actions![(OutA x)]),
        ("B", facts![(S x)], actions![(OutB x)]),
    ] {
        add_ruleset(&mut eg, set)?;
        eg.run_program(vec![Command::Rule {
            rule: tir_egglog::ast::Rule {
                span: span!(),
                name: "copy".into(),
                ruleset: set.into(),
                body: body.0,
                head,
                eval_mode: tir_egglog::ast::RuleEvalMode::Seminaive,
            },
        }])?;
    }
    run(
        &mut eg,
        vec![
            action!((R 1)),
            action!((R 2)),
            action!((R 3)),
            action!((R 4)),
            action!((S 99)),
        ],
    );
    let id = eg.add_scheduler(Box::new(DrainThree));
    eg.step_rules_with_scheduler(id, "A")?;
    assert_eq!(eg.get_size("OutA"), 3);
    eg.step_rules_with_scheduler(id, "B")?;
    assert_eq!(eg.get_size("OutA"), 3);
    let db = eg.database();
    let rows: Vec<i64> = db
        .rows(db.table("OutB")?)
        .map(|row| db.base_value(row[0]))
        .collect();
    assert_eq!(rows, [99]);
    eg.step_rules_with_scheduler(id, "A")?;
    assert_eq!(eg.get_size("OutA"), 4);
    Ok(())
}

#[test]
fn a_naive_scheduler_stops_after_its_effects_saturate() -> Result<(), Error> {
    let mut eg = EGraph::default();
    for name in ["R", "S"] {
        add_relation(&mut eg, name, vec!["i64".into()])?;
    }
    run(&mut eg, vec![action!((R 7))]);
    eg.run_program(vec![Command::Rule {
        rule: tir_egglog::ast::Rule {
            span: span!(),
            name: "copy".into(),
            ruleset: "".into(),
            eval_mode: tir_egglog::ast::RuleEvalMode::Naive,
            body: facts![(R x)].0,
            head: actions![(S x)],
        },
    }])?;
    let id = eg.add_scheduler(Box::new(FirstN(10)));
    for (updated, can_stop) in [(true, false), (false, true), (false, true)] {
        let report = eg.step_rules_with_scheduler(id, "")?;
        assert_eq!((report.updated, report.can_stop), (updated, can_stop));
        assert_eq!(eg.get_size("S"), 1);
    }
    run(&mut eg, vec![action!((R 9))]);
    assert!(eg.step_rules_with_scheduler(id, "")?.updated);
    assert_eq!(eg.get_size("S"), 2);
    Ok(())
}

#[derive(Default)]
struct DagWeights<'a> {
    weights: &'a [(&'a str, u64)],
    calls: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

impl DagWeights<'_> {
    fn weight(&self, name: &str) -> u64 {
        self.weights
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(1, |(_, c)| *c)
    }

    fn record(&self, key: String) {
        assert!(
            self.calls.borrow_mut().insert(key),
            "a marginal cost was recomputed"
        );
    }
}

impl DagCostModel<u64> for &DagWeights<'_> {
    fn base_value_cost(&self, _: &Database, sort: &ArcSort, value: Value) -> u64 {
        self.record(format!("base:{}:{value:?}", sort.name()));
        0
    }

    fn container_cost(&self, _: &Database, sort: &ArcSort, value: Value) -> u64 {
        self.record(format!("container:{}:{value:?}", sort.name()));
        0
    }

    fn enode_cost(&self, _: &Database, schema: &TableSchema, node: &Enode<'_>) -> u64 {
        self.record(format!(
            "{}:{:?}:{:?}",
            schema.name(),
            node.children,
            node.eclass
        ));
        self.weight(schema.name())
    }
}

// Recount the returned witness rather than trusting the extractor's score.
fn dag_witness_cost(
    dag: &tir_egglog::TermDag,
    roots: &[tir_egglog::TermId],
    model: &DagWeights<'_>,
) -> u64 {
    let mut pending = roots.to_vec();
    let mut seen = std::collections::BTreeSet::new();
    let mut cost = 0_u64;
    while let Some(term) = pending.pop() {
        if !seen.insert(term) {
            continue;
        }
        if let tir_egglog::Term::App(name, children) = dag.get(term) {
            cost = cost.saturating_add(model.weight(name));
            pending.extend(children.iter().copied());
        }
    }
    cost
}

fn check_dag_witness(
    eg: &mut EGraph,
    sort: &ArcSort,
    value: Value,
    dag: &tir_egglog::TermDag,
    found: &tir_egglog::extract::ExtractedTerm<u64>,
    model: &DagWeights<'_>,
) -> Result<(), Error> {
    assert_eq!(found.cost, dag_witness_cost(dag, &[found.term], model));
    let (evaluated_sort, evaluated) = eg.eval_expr(&dag.term_to_expr(&found.term, span!()))?;
    assert_eq!(evaluated_sort.name(), sort.name());
    assert_eq!(eg.database().find(evaluated), eg.database().find(value));
    Ok(())
}

#[test]
fn dag_selection_charges_shared_operations_once() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype E (Arg :cost 0) (Two) (Times E E) (Plus E E)));
    let shared = expr!((Plus(Times(Times(Arg)(Arg))(Arg))(Times(Times(Arg)(Arg))(Arg))));
    let multiply = expr!((Times(Times(Times(Arg)(Arg))(Arg))(Two)));
    run(
        &mut eg,
        vec![Action::Union(span!(), shared.clone(), multiply.clone())],
    );
    assert_eq!(
        best(&mut eg, shared.clone())?,
        (
            "(Times (Times (Times (Arg) (Arg)) (Arg)) (Two))".to_owned(),
            4
        )
    );
    let (sort, value) = eg.eval_expr(&shared)?;
    let mut dag = tir_egglog::TermDag::default();
    for (weights, expected, cost) in [
        (&[("Arg", 0)][..], shared, 3),
        (&[("Arg", 0), ("Plus", 10), ("Two", 0)][..], multiply, 3),
    ] {
        let model = DagWeights {
            weights,
            ..Default::default()
        };
        let extractor =
            GreedyDagExtractor::with_cost_model(eg.database(), &[(sort.clone(), value)], &model)?;
        let found = extractor
            .extract_best_with_sort(&mut dag, value, sort.clone())
            .unwrap();
        let again = extractor
            .extract_best_with_sort(&mut dag, value, sort.clone())
            .unwrap();
        assert_eq!(found.term, again.term);
        assert_eq!(
            (dag.to_string(found.term), found.cost),
            (expected.to_string(), cost)
        );
        drop(extractor);
        check_dag_witness(&mut eg, &sort, value, &dag, &found, &model)?;
    }
    Ok(())
}

#[test]
fn dag_snapshots_reconcile_conflicts_and_ordered_outputs() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype S (X) (D) (C S) (Bx S) (Bc S) (A S S) (Alt) (Join S S) (Out S S S)));
    run(
        &mut eg,
        vec![
            action!((union(Bx(X))(Bc(C(D))))),
            action!((union(A(Bx(X))(X))(Alt))),
        ],
    );
    let weights = &[
        ("X", 10),
        ("D", 1),
        ("C", 4),
        ("Bx", 1),
        ("Bc", 1),
        ("A", 1),
        ("Alt", 15),
        ("Join", 0),
        ("Out", 0),
    ];
    let a = expr!((A(Bx(X))(X)));
    let b = expr!((Bx(X)));
    for (expr, expected) in [
        (a.clone(), "(A (Bx (X)) (X))"),
        (
            expr!((Join(A(Bx(X))(X))(Bx(X)))),
            "(Join (A (Bx (X)) (X)) (Bx (X)))",
        ),
        (
            expr!((Out(A(Bx(X))(X))(Bx(X))(A(Bx(X))(X)))),
            "(Out (A (Bx (X)) (X)) (Bx (X)) (A (Bx (X)) (X)))",
        ),
    ] {
        let (sort, value) = eg.eval_expr(&expr)?;
        let model = DagWeights {
            weights,
            ..Default::default()
        };
        let extractor =
            GreedyDagExtractor::with_cost_model(eg.database(), &[(sort.clone(), value)], &model)?;
        let mut dag = tir_egglog::TermDag::default();
        let found = extractor
            .extract_best_with_sort(&mut dag, value, sort.clone())
            .unwrap();
        assert_eq!(
            (dag.to_string(found.term), found.cost),
            (expected.to_owned(), 12)
        );
        if let tir_egglog::Term::App(name, children) = dag.get(found.term) {
            if name == "Out" {
                assert_eq!(children[0], children[2]);
                assert_eq!(dag.to_string(children[1]), "(Bx (X))");
            }
        }
        drop(extractor);
        check_dag_witness(&mut eg, &sort, value, &dag, &found, &model)?;
    }
    // Independent roots may keep different producers for B in their snapshots.
    let roots = [eg.eval_expr(&a)?, eg.eval_expr(&b)?, eg.eval_expr(&a)?];
    let mut dag = tir_egglog::TermDag::default();
    let mut selected = Vec::new();
    // One preparation, independent extraction calls, and one shared termdag.
    let model = DagWeights {
        weights,
        ..Default::default()
    };
    let extractor = GreedyDagExtractor::with_cost_model(eg.database(), &roots, &model)?;
    for (sort, value) in &roots {
        selected.push(
            extractor
                .extract_best_with_sort(&mut dag, *value, sort.clone())
                .unwrap(),
        );
    }
    assert_eq!(selected.iter().map(|term| term.cost).sum::<u64>(), 30);
    assert_eq!(
        dag_witness_cost(
            &dag,
            &selected.iter().map(|term| term.term).collect::<Vec<_>>(),
            &model
        ),
        18
    );
    Ok(())
}

#[test]
fn dag_finite_witnesses_survive_zero_max_and_signed_cycles() -> Result<(), Error> {
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype Z (Leaf) (Cycle Z) (Seed) (Spin Z)));
    let seed = eg.database().table("Seed")?;
    eg.database_mut().set_unextractable(seed, true);
    run(
        &mut eg,
        vec![
            action!((union(Leaf)(Cycle(Leaf)))),
            action!((union(Seed)(Spin(Seed)))),
        ],
    );
    let (sort, value) = eg.eval_expr(&expr!((Leaf)))?;
    let (_, spin) = eg.eval_expr(&expr!((Seed)))?;
    for cost in [0, u64::MAX] {
        let model = DagWeights {
            weights: &[("Leaf", cost), ("Cycle", 0)],
            ..Default::default()
        };
        let extractor = GreedyDagExtractor::with_cost_model(
            eg.database(),
            &[(sort.clone(), value), (sort.clone(), spin)],
            &model,
        )?;
        let mut dag = tir_egglog::TermDag::default();
        let found = extractor
            .extract_best_with_sort(&mut dag, value, sort.clone())
            .unwrap();
        assert_eq!(
            (dag.to_string(found.term), found.cost),
            ("(Leaf)".to_owned(), cost)
        );
        assert!(extractor
            .extract_best_with_sort(&mut dag, spin, sort.clone())
            .is_none());
        drop(extractor);
        check_dag_witness(&mut eg, &sort, value, &dag, &found, &model)?;
    }
    // Signed exact addition is an ordered commutative monoid. Even a negative
    // cycle must not replace the finite leaf with a recursive witness.
    struct Signed;
    impl DagCostModel<tir_egglog::sort::BigInt> for Signed {
        fn base_value_cost(&self, _: &Database, _: &ArcSort, _: Value) -> tir_egglog::sort::BigInt {
            tir_egglog::sort::BigInt::new(0)
        }
        fn enode_cost(
            &self,
            _: &Database,
            t: &TableSchema,
            _: &Enode<'_>,
        ) -> tir_egglog::sort::BigInt {
            tir_egglog::sort::BigInt::new(if t.name() == "Leaf" { -2 } else { -1 })
        }
    }
    let extractor =
        GreedyDagExtractor::with_cost_model(eg.database(), &[(sort.clone(), value)], Signed)?;
    let mut dag = tir_egglog::TermDag::default();
    let found = extractor
        .extract_best_with_sort(&mut dag, value, sort)
        .unwrap();
    assert_eq!(found.cost, tir_egglog::sort::BigInt::new(-2));
    assert_eq!(dag.to_string(found.term), "(Leaf)");
    Ok(())
}

#[test]
fn dag_keys_keep_sorts_distinct_and_filter_invisible_producers() -> Result<(), Error> {
    use tir_egglog::TermDag;
    let mut eg = EGraph::default();
    datatype!(&mut eg, (datatype E (Visible i64) (Cheap :cost 0) (Hidden)));
    datatype!(&mut eg, (datatype Collision (Pair E i64)));
    let hidden_table = eg.database().table("Hidden")?;
    eg.database_mut().set_unextractable(hidden_table, true);
    let (sort, root) = eg.eval_expr(&expr!((Visible 0)))?;
    let (_, cheap) = eg.eval_expr(&expr!((Cheap)))?;
    let (_, hidden) = eg.eval_expr(&expr!((Hidden)))?;
    let db = eg.database_mut();
    let internal = db.add_constructor("@internal", &[], db.sort("E")?);
    let internal_value = db.entry(internal, &[])?;
    db.union(root, cheap);
    db.union(root, hidden);
    db.union(root, internal_value);
    db.subsume(db.table("Cheap")?, &[]);
    // Eq IDs and unboxed primitive cells deliberately have the same raw value.
    let primitive = db.base(i64::from(root.raw()));
    assert_eq!(primitive, root);
    let collision = db.sort("Collision")?;
    let pair = db.table("Pair")?;
    let pair_value = db.entry(pair, &[root, primitive])?;
    db.rebuild()?;
    let pair_sort = db.schema().sort(collision).arcsort().clone();
    let model = DagWeights {
        weights: &[
            ("Visible", 7),
            ("@internal", 0),
            ("Hidden", 0),
            ("Cheap", 0),
            ("Pair", 0),
        ],
        ..Default::default()
    };
    let extractor =
        GreedyDagExtractor::with_cost_model(db, &[(pair_sort.clone(), pair_value)], &model)?;
    let mut dag = TermDag::default();
    let found = extractor
        .extract_best_with_sort(&mut dag, pair_value, pair_sort.clone())
        .unwrap();
    assert_eq!(found.cost, 7);
    assert_eq!(
        dag.to_string(found.term),
        format!("(Pair (Visible 0) {})", root.raw())
    );
    assert!(extractor
        .extract_best_with_sort(&mut dag, hidden, sort)
        .is_some());
    drop(extractor);
    check_dag_witness(&mut eg, &pair_sort, pair_value, &dag, &found, &model)?;
    Ok(())
}

#[test]
fn dag_reconstruction_uses_a_stack_for_deep_witnesses() -> Result<(), Error> {
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(|| -> Result<(), Error> {
            let mut db = Database::new();
            let sort = db.add_sort("Deep", tir_egglog::SortKind::Eq);
            let leaf = db.add_constructor("Leaf", &[], sort);
            let next = db.add_constructor("Next", &[sort], sort);
            let mut root = db.entry(leaf, &[])?;
            for _ in 0..1024 {
                root = db.entry(next, &[root])?;
            }
            db.rebuild()?;
            let sort = db.schema().sort(sort).arcsort().clone();
            let extractor = GreedyDagExtractor::with_cost_model(
                &db,
                &[(sort.clone(), root)],
                tir_egglog::extract::AdditiveCostModel::default(),
            )?;
            let mut dag = tir_egglog::TermDag::default();
            let found = extractor
                .extract_best_with_sort(&mut dag, root, sort)
                .unwrap();
            assert_eq!(found.cost, 1025);
            let mut term = found.term;
            let mut depth = 0;
            while let tir_egglog::Term::App(name, children) = dag.get(term) {
                if name == "Leaf" {
                    break;
                }
                assert_eq!(name, "Next");
                term = children[0];
                depth += 1;
            }
            assert_eq!(depth, 1024);
            Ok(())
        })
        .unwrap()
        .join()
        .unwrap()
}
