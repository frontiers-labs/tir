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
    program.run(&mut fresh, &tir_egglog::Schedule::run(""))?;
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
