use std::io::{self, Write};

use serde::Serialize;

use super::{INF_COST, PbqpEdge, PbqpProblem};

#[derive(Serialize)]
struct ProblemDump<'a> {
    format: &'static str,
    version: u8,
    kind: &'a str,
    inf_cost: u64,
    node_costs: &'a [Vec<u64>],
    edges: Vec<EdgeDump>,
    matrices: Vec<MatrixDump<'a>>,
}

#[derive(Serialize)]
struct EdgeDump {
    lhs: u32,
    rhs: u32,
    matrix: usize,
}

#[derive(Serialize)]
struct MatrixDump<'a> {
    rows: usize,
    cols: usize,
    costs: &'a [u64],
}

impl PbqpProblem {
    /// Write the mathematical PBQP task as version 1 JSON.
    pub fn write_json(&self, writer: impl Write, kind: &str) -> io::Result<()> {
        let mut source_edges: Vec<&PbqpEdge> = self.edges.iter().collect();
        source_edges.sort_unstable_by_key(|edge| (edge.lhs, edge.rhs));

        let mut source_matrix_ids = vec![None; self.matrices.matrices.len()];
        let mut matrices = Vec::new();
        let edges = source_edges
            .into_iter()
            .map(|edge| {
                let source_id = edge.matrix.0 as usize;
                let matrix = *source_matrix_ids[source_id].get_or_insert_with(|| {
                    let id = matrices.len();
                    let source = self.matrices.get(edge.matrix);
                    matrices.push(MatrixDump {
                        rows: source.rows,
                        cols: source.cols,
                        costs: &source.costs,
                    });
                    id
                });
                EdgeDump {
                    lhs: edge.lhs,
                    rhs: edge.rhs,
                    matrix,
                }
            })
            .collect();

        serde_json::to_writer(
            writer,
            &ProblemDump {
                format: "tir-pbqp",
                version: 1,
                kind,
                inf_cost: INF_COST,
                node_costs: &self.node_costs,
                edges,
                matrices,
            },
        )
        .map_err(Into::into)
    }
}
