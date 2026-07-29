use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tir::pbqp::{PbqpMatrix, PbqpNodeId, PbqpProblem, solve};

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

fn bench_dense_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("pbqp/dense_search");
    for node_count in [16, 32] {
        let problem = dense_problem(node_count, 4);
        group.bench_with_input(
            BenchmarkId::from_parameter(node_count),
            &problem,
            |b, problem| {
                b.iter_batched(
                    || problem.clone(),
                    |problem| black_box(solve(&problem).expect("PBQP should be solvable")),
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_dense_search);
criterion_main!(benches);
