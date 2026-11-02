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
