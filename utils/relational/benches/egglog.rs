//! egglog counterpart of the `egraph` benchmark: the same workloads (see
//! [`shared`]), seed graphs, rules and iteration counts, run on egglog.
//!
//! A workload becomes an egglog program: one constructor per operator and
//! arity, one `rewrite` per rule, and the seed graph as `let` and `union`
//! commands. Only the `run` is measured. The engine is serial, as the other two
//! are.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use egglog::EGraph;
use tir_bench::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

#[path = "workload.rs"]
mod shared;
use shared::{Pattern, Workload};

fn constructor(symbol: u32, arity: usize) -> String {
    format!("S{symbol}a{arity}")
}

/// The pattern as an egglog expression; a hole is a bare variable.
fn expr(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Var(var) => format!("v{var}"),
        Pattern::Node(symbol, children) => {
            let mut out = format!("({}", constructor(*symbol, children.len()));
            for child in children {
                out.push(' ');
                out.push_str(&expr(child));
            }
            out.push(')');
            out
        }
    }
}

fn constructors(pattern: &Pattern, out: &mut BTreeSet<(u32, usize)>) {
    if let Pattern::Node(symbol, children) = pattern {
        out.insert((*symbol, children.len()));
        children.iter().for_each(|child| constructors(child, out));
    }
}

/// The names of the workload's constructors, and an e-graph holding its seed
/// graph and rules.
fn seed(workload: &Workload) -> (Vec<String>, EGraph) {
    let mut used = BTreeSet::new();
    for seed in &workload.seeds {
        used.insert((seed.symbol, seed.children.len()));
    }
    for rule in &workload.rules {
        constructors(&rule.lhs, &mut used);
        constructors(&rule.rhs, &mut used);
    }
    let mut program = String::from("(datatype T");
    for &(symbol, arity) in &used {
        write!(
            program,
            " ({}{})",
            constructor(symbol, arity),
            " T".repeat(arity)
        )
        .unwrap();
    }
    program.push_str(")\n(ruleset rw)\n");
    for rule in &workload.rules {
        writeln!(
            program,
            "(rewrite {} {} :ruleset rw)",
            expr(&rule.lhs),
            expr(&rule.rhs)
        )
        .unwrap();
    }
    let mut defined = vec![false; workload.classes];
    for seed in &workload.seeds {
        let mut term = format!("({}", constructor(seed.symbol, seed.children.len()));
        for child in &seed.children {
            write!(term, " $c{child}").unwrap();
        }
        term.push(')');
        if std::mem::replace(&mut defined[seed.class as usize], true) {
            writeln!(program, "(union $c{} {term})", seed.class).unwrap();
        } else {
            writeln!(program, "(let $c{} {term})", seed.class).unwrap();
        }
    }
    let mut egraph = EGraph::new(1);
    egraph
        .parse_and_run_program(None, &program)
        .expect("workload is a valid egglog program");
    let names = used
        .iter()
        .map(|&(symbol, arity)| constructor(symbol, arity))
        .collect();
    (names, egraph)
}

fn saturate(mut egraph: EGraph, iters: usize) -> EGraph {
    egraph
        .parse_and_run_program(None, &format!("(run-schedule (repeat {iters} (run rw)))"))
        .expect("saturation runs");
    egraph
}

/// The same table the `egraph` benchmark prints under `TIR_WORKLOAD_STATS`.
/// egglog does not count classes; its node count is egg's.
fn print_stats(workloads: &[Workload]) {
    if std::env::var_os("TIR_WORKLOAD_STATS").is_none() {
        return;
    }
    for workload in workloads {
        let (names, seeded) = seed(workload);
        for iters in 0..=workload.iters {
            let g = saturate(seeded.clone(), iters);
            let nodes: usize = names.iter().map(|name| g.get_size(name)).sum();
            eprintln!(
                "stats engine=egglog workload={} iters={iters} nodes={nodes}",
                workload.name
            );
        }
    }
}

fn benches(c: &mut Criterion) {
    let workloads = shared::workloads();
    print_stats(&workloads);
    let mut group = c.benchmark_group("egglog/saturate");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            let (_, seeded) = seed(workload);
            b.iter_batched(
                || seeded.clone(),
                |egraph| saturate(egraph, workload.iters),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(all, benches);
criterion_main!(all);
