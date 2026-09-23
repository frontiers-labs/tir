use std::hint::black_box;

use tir_bench::Suite;
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

fn bench_block_search(suite: &mut Suite) -> tir_bench::Result<()> {
    for node_count in [512, 4096] {
        let name = format!("block_search/{node_count}");
        if suite.options().list {
            suite.list_function(&name)?;
            continue;
        }
        if !suite.matches(&name) {
            continue;
        }
        let problem = block_problem(node_count, 8);
        suite.function(&name, |b| {
            b.iter_batched(
                || problem.clone(),
                |problem| black_box(solve(&problem).expect("PBQP should be solvable")),
            );
        })?;
    }
    Ok(())
}

fn bench_dense_search(suite: &mut Suite) -> tir_bench::Result<()> {
    for node_count in [16, 32] {
        let name = format!("dense_search/{node_count}");
        if suite.options().list {
            suite.list_function(&name)?;
            continue;
        }
        if !suite.matches(&name) {
            continue;
        }
        let problem = dense_problem(node_count, 4);
        suite.function(&name, |b| {
            b.iter_batched(
                || problem.clone(),
                |problem| black_box(solve(&problem).expect("PBQP should be solvable")),
            );
        })?;
    }
    Ok(())
}

fn main() -> tir_bench::Result<()> {
    let mut suite = Suite::from_args(concat!(env!("CARGO_PKG_NAME"), "/pbqp"))?;
    bench_dense_search(&mut suite)?;
    bench_block_search(&mut suite)?;
    suite.finish()
}
