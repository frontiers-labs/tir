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
