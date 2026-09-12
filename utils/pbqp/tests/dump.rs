use serde::Deserialize;
use std::io::{self, Write};
use tir_pbqp::{INF_COST, PbqpMatrix, PbqpNodeId, PbqpProblem, solve};

#[derive(Deserialize)]
struct Dump {
    node_costs: Vec<Vec<u64>>,
    edges: Vec<Edge>,
    matrices: Vec<Matrix>,
}

#[derive(Deserialize)]
struct Edge {
    lhs: usize,
    rhs: usize,
    matrix: usize,
}

#[derive(Deserialize)]
struct Matrix {
    rows: usize,
    cols: usize,
    costs: Vec<u64>,
}

#[test]
fn writes_exact_costs_and_orients_merged_edges() {
    let mut problem = PbqpProblem::new();
    let lhs = problem.add_node(vec![0, INF_COST, u64::MAX]);
    let rhs = problem.add_node(vec![7, 9]);
    problem.add_edge(
        rhs,
        lhs,
        PbqpMatrix::new(2, 3, vec![1, 2, 3, 4, 5, u64::MAX]),
    );
    problem.add_edge(
        lhs,
        rhs,
        PbqpMatrix::new(3, 2, vec![10, 20, 30, 40, 50, 60]),
    );

    let mut json = Vec::new();
    problem.write_json(&mut json, "test").unwrap();

    assert_eq!(
        String::from_utf8(json).unwrap(),
        r#"{"format":"tir-pbqp","version":1,"kind":"test","inf_cost":4611686018427387903,"node_costs":[[0,4611686018427387903,18446744073709551615],[7,9]],"edges":[{"lhs":0,"rhs":1,"matrix":0}],"matrices":[{"rows":3,"cols":2,"costs":[11,24,32,45,53,4611686018427387903]}]}"#
    );
}

#[test]
fn orders_edges_and_emits_only_live_shared_matrices() {
    let mut problem = PbqpProblem::new();
    let nodes: Vec<_> = (0..4).map(|_| problem.add_node(vec![0, 0])).collect();
    let shared = PbqpMatrix::new(2, 2, vec![0, u64::MAX, 1, 0]);

    problem.add_edge(
        nodes[2],
        nodes[3],
        PbqpMatrix::new(2, 2, vec![99, 99, 99, 99]),
    );
    problem.add_edge(nodes[1], nodes[3], shared.clone());
    problem.add_edge(nodes[0], nodes[2], shared);
    problem.add_edge(nodes[2], nodes[3], PbqpMatrix::new(2, 2, vec![1, 1, 1, 1]));

    let mut bytes = Vec::new();
    problem.write_json(&mut bytes, "test").unwrap();
    let dump: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        dump["edges"],
        serde_json::json!([
            {"lhs": 0, "rhs": 2, "matrix": 0},
            {"lhs": 1, "rhs": 3, "matrix": 0},
            {"lhs": 2, "rhs": 3, "matrix": 1}
        ])
    );
    assert_eq!(
        dump["matrices"],
        serde_json::json!([
            {"rows": 2, "cols": 2, "costs": [0, 18446744073709551615_u64, 1, 0]},
            {"rows": 2, "cols": 2, "costs": [100, 100, 100, 100]}
        ])
    );
}

#[test]
fn writing_preserves_the_problem_and_reconstructs_its_objective() {
    let mut problem = PbqpProblem::new();
    let a = problem.add_node(vec![3, 0]);
    let b = problem.add_node(vec![0, 2, 4]);
    problem.add_edge(a, b, PbqpMatrix::new(2, 3, vec![0, 8, 1, 7, 0, 6]));
    let original = problem.clone();
    let expected_solution = solve(&problem).unwrap();

    let mut bytes = Vec::new();
    problem.write_json(&mut bytes, "test").unwrap();
    assert_eq!(problem, original);

    let dump: Dump = serde_json::from_slice(&bytes).unwrap();
    let mut reconstructed = PbqpProblem::new();
    for costs in dump.node_costs {
        reconstructed.add_node(costs);
    }
    for edge in dump.edges {
        let matrix = &dump.matrices[edge.matrix];
        reconstructed.add_edge(
            PbqpNodeId::from_index(edge.lhs),
            PbqpNodeId::from_index(edge.rhs),
            PbqpMatrix::new(matrix.rows, matrix.cols, matrix.costs.clone()),
        );
    }

    assert_eq!(solve(&reconstructed).unwrap(), expected_solution);
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn returns_the_underlying_writer_error() {
    let problem = PbqpProblem::new();

    let error = problem.write_json(FailingWriter, "test").unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}
