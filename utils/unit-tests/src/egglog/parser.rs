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
