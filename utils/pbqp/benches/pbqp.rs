use std::hint::black_box;

use tir_bench::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
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

fn dense_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("pbqp/dense_search");
    for nodes in [16, 32] {
        let problem = dense_problem(nodes, 4);
        group.bench_with_input(
            BenchmarkId::from_parameter(nodes),
            &problem,
            |b, problem| {
                b.iter_batched(
                    || problem.clone(),
                    |problem| solve_problem(&problem),
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn block_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("pbqp/block_search");
    group.sample_size(20);
    for nodes in [512, 4096] {
        let problem = block_problem(nodes, 8);
        group.bench_with_input(
            BenchmarkId::from_parameter(nodes),
            &problem,
            |b, problem| {
                b.iter_batched(
                    || problem.clone(),
                    |problem| solve_problem(&problem),
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, dense_search, block_search);
criterion_main!(benches);
