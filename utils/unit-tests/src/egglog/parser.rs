use tir_egglog::{EGraph, Error};

#[test]
fn parsed_program_executes_rules_and_reports_unfinished_input() {
    let mut graph = EGraph::default();
    let program = graph
        .parse_program(
            "program.egg",
            "(relation source (i64))
(relation result (i64))
(source 7)
(rule ((source x)) ((result (+ x 1))))
(run 1)
(check (result 8))",
        )
        .unwrap();
    graph.run_program(program).unwrap();
    let db = graph.database();
    let rows: Vec<i64> = db
        .rows(db.table("result").unwrap())
        .map(|row| db.base_value(row[0]))
        .collect();
    assert_eq!(rows, [8]);
    assert!(matches!(
        graph.parse_program("unfinished.egg", "(relation"),
        Err(Error::Unfinished(_))
    ));
}

#[test]
fn wildcards_do_not_capture_user_variables_or_globals() {
    for (prefix, pattern, result, expected) in [
        ("", "_ _1", "_1", 2),
        ("", "_1 _", "_1", 1),
        ("", "_ _", "42", 42),
        ("(let _1 99)", "_ value", "value", 2),
        ("(let $_1 99)", "_ value", "value", 2),
        ("(sort _1)", "_ value", "value", 2),
        ("(relation _1 (i64))", "_ value", "value", 2),
        ("(ruleset _1)", "_ value", "value", 2),
    ] {
        let mut graph = EGraph::default();
        let bindings = graph.parse_program("bindings.egg", prefix).unwrap();
        graph.run_program(bindings).unwrap();
        let text = format!(
            "(relation r (i64 i64)) (relation result (i64)) (r 1 2) \
             (rule ((r {pattern})) ((result {result}))) (run 1)"
        );
        let program = graph.parse_program("wildcards.egg", &text).unwrap();
        graph.run_program(program).unwrap();
        let db = graph.database();
        let rows: Vec<i64> = db
            .rows(db.table("result").unwrap())
            .map(|row| db.base_value(row[0]))
            .collect();
        assert_eq!(rows, [expected], "{prefix}; {pattern}");
    }
}

#[test]
fn same_program_global_alias_cannot_capture_a_wildcard() {
    let mut graph = EGraph::default();
    let program = graph
        .parse_program(
            "alias.egg",
            "(let $_1 99) (relation r (i64)) (r 1) (rule ((r _)) ((r 2))) (run 1)",
        )
        .unwrap();
    graph.run_program(program).unwrap();
    let db = graph.database();
    let mut rows: Vec<i64> = db
        .rows(db.table("r").unwrap())
        .map(|row| db.base_value(row[0]))
        .collect();
    rows.sort_unstable();
    assert_eq!(rows, [1, 2]);
}

#[test]
fn conflicting_merge_annotations_are_rejected() {
    for options in [
        ":merge (+ old new) :no-merge",
        ":no-merge :merge (+ old new)",
    ] {
        let graph = EGraph::default();
        let text = format!("(function f () i64 {options})");
        assert!(matches!(
            graph.parse_program("conflict.egg", &text),
            Err(Error::Parse(_))
        ));
    }
    for (options, succeeds, expected) in [(":merge (+ old new)", true, 5), (":no-merge", false, 2)]
    {
        let mut graph = EGraph::default();
        let text = format!("(function f () i64 {options}) (set (f) 2)");
        let program = graph.parse_program("function.egg", &text).unwrap();
        graph.run_program(program).unwrap();
        let write = graph.parse_program("write.egg", "(set (f) 3)").unwrap();
        assert_eq!(graph.run_program(write).is_ok(), succeeds);
        let db = graph.database();
        let output = db.lookup(db.table("f").unwrap(), &[]).unwrap();
        assert_eq!(db.base_value::<i64>(output), expected);
    }
}

#[test]
fn registered_commands_override_builtin_text_forms() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tir_egglog::ast::{Expr, Literal};
    use tir_egglog::{CommandOutput, UserDefinedCommand};

    struct Handler(Arc<AtomicUsize>);
    impl UserDefinedCommand for Handler {
        fn update(&self, _: &mut EGraph, args: &[Expr]) -> Result<Vec<CommandOutput>, Error> {
            assert!(matches!(args, [Expr::Lit(_, Literal::Int(1))]));
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }
    for name in ["run", "check", "sort"] {
        let mut graph = EGraph::default();
        let calls = Arc::new(AtomicUsize::new(0));
        graph
            .add_command(name.to_owned(), Arc::new(Handler(calls.clone())))
            .unwrap();
        let text = format!("({name} 1)");
        let program = graph.parse_program("user.egg", &text).unwrap();
        graph.run_program(program).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{name}");
    }
}

#[test]
fn command_named_tables_are_valid_in_facts_and_rule_actions() {
    for name in ["run", "sort", "rule", "function"] {
        for constructor in [false, true] {
            let mut graph = EGraph::default();
            let declaration = if constructor {
                format!("(datatype K ({name} i64))")
            } else {
                format!("(relation {name} (i64))")
            };
            let text = format!(
                "{declaration} (relation Seed (i64)) (relation Out (i64)) \
                 (rule ((Seed x)) (({name} x))) (rule (({name} x)) ((Out x))) \
                 (Seed 1) (run 2)"
            );
            let program = graph.parse_program("contexts.egg", &text).unwrap();
            graph.run_program(program).unwrap();
            let db = graph.database();
            let rows: Vec<i64> = db
                .rows(db.table("Out").unwrap())
                .map(|row| db.base_value(row[0]))
                .collect();
            assert_eq!(rows, [1], "{name}; constructor={constructor}");
        }
    }
    let graph = EGraph::default();
    for text in [
        "(function 1)",
        "(check (= x))",
        "(rule ((Seed x)) ((set x)))",
    ] {
        assert!(graph.parse_program("invalid.egg", text).is_err(), "{text}");
    }
}
