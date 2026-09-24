#[macro_use]
#[path = "../../../benchmarks/functions.rs"]
pub mod functions;

use std::hint::black_box;
use tir_pbqp::{PbqpMatrix, PbqpNodeId, PbqpProblem, solve};

fn dense_problem(node_count: usize, alternative_count: usize) -> PbqpProblem {
    let mut problem = PbqpProblem::new();
    for _ in 0..node_count {
        problem.add_node((0..alternative_count as u64).collect());
    }

    let matrix = PbqpMatrix::new(
        alternative_count,
        alternative_count,
        (0..alternative_count)
            .flat_map(|row| (0..alternative_count).map(move |col| if row == col { 1 } else { 0 }))
            .collect(),
    );
    for lhs in 0..node_count {
        for rhs in lhs + 1..node_count {
            problem.add_edge(
                PbqpNodeId::from_index(lhs),
                PbqpNodeId::from_index(rhs),
                matrix.clone(),
            );
        }
    }
    problem
}

/// The shape a large basic block produces: many sparse nodes, a tree of
/// expression classes plus sibling constraints, so exact reductions cascade and
/// R2 introduces fill-in edges between the surviving neighbors.
fn block_problem(node_count: usize, alternative_count: usize) -> PbqpProblem {
    let mut problem = PbqpProblem::new();
    for node in 0..node_count {
        problem.add_node(
            (0..alternative_count)
                .map(|alternative| ((node + alternative) % 7) as u64)
                .collect(),
        );
    }

    let matrix = |seed: usize| {
        PbqpMatrix::new(
            alternative_count,
            alternative_count,
            (0..alternative_count)
                .flat_map(|row| {
                    (0..alternative_count).map(move |col| ((row * 3 + col * 5 + seed) % 11) as u64)
                })
                .collect(),
        )
    };

    for node in 1..node_count {
        problem.add_edge(
            PbqpNodeId::from_index(node),
            PbqpNodeId::from_index((node - 1) / 2),
            matrix(node),
        );
        if node % 2 == 1 && node + 1 < node_count {
            problem.add_edge(
                PbqpNodeId::from_index(node),
                PbqpNodeId::from_index(node + 1),
                matrix(node + 2),
            );
        }
    }
    problem
}

fn solve_problem(problem: &PbqpProblem) {
    black_box(solve(problem).expect("PBQP should be solvable"));
}

fn bench_dense(b: &mut functions::Bencher<'_, '_>, nodes: usize) {
    let problem = dense_problem(nodes, 4);
    b.iter_batched(|| problem.clone(), |problem| solve_problem(&problem));
}

fn bench_block(b: &mut functions::Bencher<'_, '_>, nodes: usize) {
    let problem = block_problem(nodes, 8);
    b.iter_batched(|| problem.clone(), |problem| solve_problem(&problem));
}

fn block_settings() -> functions::Settings {
    functions::Settings {
        samples: Some(20),
        ..Default::default()
    }
}

benchmarks! {
    compiler = "tir";
    dense_search_16("pbqp/dense_search/16") |b| { bench_dense(b, 16); }
    dense_search_32("pbqp/dense_search/32") |b| { bench_dense(b, 32); }
    block_search_512("pbqp/block_search/512", block_settings()) |b| { bench_block(b, 512); }
    block_search_4096("pbqp/block_search/4096", block_settings()) |b| { bench_block(b, 4096); }
}
