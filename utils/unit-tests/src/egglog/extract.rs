//! Extraction with `:cost` and `:unextractable`, variants and a custom cost
//! model; extraction attributes and defaults across a program file; and
//! schedulers (egglog's src/scheduler.rs tests).

use std::sync::Arc;

use tir_egglog::ast::{Command, Schema};
use tir_egglog::extract::TreeCostModel;
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
        }
        let sort = db.schema().sort(root_sort).arcsort().clone();
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
        let extracted = extractor.extract_best_with_sort(&mut terms, hidden_root, sort);
        if local {
            let extracted = extracted.unwrap();
            assert_eq!(
                (terms.to_string(extracted.term), extracted.cost),
                ("(Only (map-of (A) (A)))".to_owned(), 5)
            );
        } else {
            assert!(extracted.is_none());
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
