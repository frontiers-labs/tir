//! egg counterpart of the `egraph` benchmark: the same workloads (see
//! [`shared`]), seed graphs, rules and iteration counts, run on egg.
//!
//! The language is an operator index with inline children rather than egg's
//! `SymbolLang`, so egg is not charged for a `Vec` per e-node. `SimpleScheduler`
//! applies every match each iteration, as the other two engines do.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::hint::black_box;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use egg::{
    AstSize, EGraph, Extractor, FromOp, Id, Language, Pattern, Rewrite, Runner, SimpleScheduler,
};
use smallvec::SmallVec;
use tir_bench::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

#[path = "workload.rs"]
mod shared;
use shared::Workload;

/// Operator name -> index, for the patterns egg parses from text. The index is
/// private to this table: a seed node is built from its name too.
fn intern(name: &str) -> u32 {
    static TABLE: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    let mut table = TABLE.get_or_init(Default::default).lock().unwrap();
    let next = table.len() as u32;
    *table.entry(name.to_string()).or_insert(next)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Sym {
    op: u32,
    children: SmallVec<[Id; 4]>,
}

impl Language for Sym {
    type Discriminant = (u32, usize);

    fn discriminant(&self) -> Self::Discriminant {
        (self.op, self.children.len())
    }

    fn matches(&self, other: &Self) -> bool {
        self.discriminant() == other.discriminant()
    }

    fn children(&self) -> &[Id] {
        &self.children
    }

    fn children_mut(&mut self) -> &mut [Id] {
        &mut self.children
    }
}

impl fmt::Display for Sym {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "op{}", self.op)
    }
}

impl FromOp for Sym {
    type Error = Infallible;

    fn from_op(op: &str, children: Vec<Id>) -> Result<Self, Self::Error> {
        Ok(Sym {
            op: intern(op),
            children: children.into(),
        })
    }
}

fn build_rules(workload: &Workload) -> Vec<Rewrite<Sym, ()>> {
    let plain = |name: &str| name.to_string();
    workload
        .rules
        .iter()
        .map(|spec| {
            let lhs: Pattern<Sym> = spec.lhs.sexp(&workload.symbols, &plain).parse().unwrap();
            let rhs: Pattern<Sym> = spec.rhs.sexp(&workload.symbols, &plain).parse().unwrap();
            Rewrite::new(spec.name.as_str(), lhs, rhs).unwrap()
        })
        .collect()
}

fn seed(workload: &Workload) -> EGraph<Sym, ()> {
    let ops: Vec<u32> = workload.symbols.iter().map(|name| intern(name)).collect();
    let mut g = EGraph::default();
    let mut classes: Vec<Option<Id>> = vec![None; workload.classes];
    for seed in &workload.seeds {
        let id = g.add(Sym {
            op: ops[seed.symbol as usize],
            children: seed
                .children
                .iter()
                .map(|&child| classes[child as usize].expect("child class is defined"))
                .collect(),
        });
        match classes[seed.class as usize] {
            Some(class) => {
                g.union(class, id);
            }
            None => classes[seed.class as usize] = Some(id),
        }
    }
    g.rebuild();
    g
}

fn runner(workload: &Workload, iters: usize) -> Runner<Sym, ()> {
    Runner::default()
        .with_egraph(seed(workload))
        .with_iter_limit(iters)
        .with_node_limit(10_000_000)
        .with_time_limit(Duration::from_secs(600))
        .with_scheduler(SimpleScheduler)
}

fn ematch_all(rules: &[Rewrite<Sym, ()>], egraph: &EGraph<Sym, ()>) -> usize {
    let mut total = 0usize;
    for rule in rules {
        for matched in rule.search(egraph) {
            total += black_box(matched.substs.len());
        }
    }
    total
}

fn extract_all(egraph: &EGraph<Sym, ()>) -> usize {
    let extractor = Extractor::new(egraph, AstSize);
    let mut total = 0usize;
    for class in egraph.classes() {
        total += black_box(extractor.find_best_cost(class.id));
    }
    total
}

/// The same table the `egraph` benchmark prints under `TIR_WORKLOAD_STATS`.
fn print_stats(workloads: &[Workload]) {
    if std::env::var_os("TIR_WORKLOAD_STATS").is_none() {
        return;
    }
    for workload in workloads {
        let rules = build_rules(workload);
        for iters in 0..=workload.iters {
            let g = runner(workload, iters).run(&rules).egraph;
            eprintln!(
                "stats engine=egg workload={} iters={iters} classes={} nodes={}",
                workload.name,
                g.number_of_classes(),
                g.total_number_of_nodes()
            );
        }
    }
}

fn benches(c: &mut Criterion) {
    let workloads = shared::workloads();
    // egg calibrates its clock on first use, in a loop bounded by wall time.
    // Run it before anything is measured so instruction counts stay repeatable.
    runner(&workloads[0], 1).run(&build_rules(&workloads[0]));
    print_stats(&workloads);
    let mut group = c.benchmark_group("egg/seed");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            b.iter(|| seed(workload));
        });
    }
    group.finish();
    let mut group = c.benchmark_group("egg/saturate");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            let rules = build_rules(workload);
            b.iter_batched(
                || runner(workload, workload.iters),
                |runner| runner.run(&rules),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
    let mut group = c.benchmark_group("egg/ematch");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            let rules = build_rules(workload);
            let g = runner(workload, workload.iters).run(&rules).egraph;
            b.iter(|| ematch_all(&rules, &g));
        });
    }
    group.finish();
    let mut group = c.benchmark_group("egg/extract");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            let rules = build_rules(workload);
            let g = runner(workload, workload.iters).run(&rules).egraph;
            b.iter(|| extract_all(&g));
        });
    }
    group.finish();
}

criterion_group!(all, benches);
criterion_main!(all);
