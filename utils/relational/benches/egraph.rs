//! Equality saturation on `tir-relational`, over the workloads `fcc` produces
//! while compiling real programs (see [`shared`]). The `egg` and `egglog`
//! benchmarks run the same workloads on those engines.

use std::hash::{Hash, Hasher};
use std::hint::black_box;

use smallvec::SmallVec;
use tir_adt::FxHasher;
use tir_bench::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tir_relational::{Atom, HeadOp, LabelFill, NoExterns, Plan, Query, Rule};
use tir_relational::{ClassId as Id, Engine, Label as ENode};

#[path = "workload.rs"]
mod shared;
use shared::{Pattern, RuleSpec, Workload};

const NODE_LIMIT: usize = 10_000_000;

/// An operator by index into the workload's symbol table.
#[derive(Clone, Debug)]
struct Sym {
    op: u32,
    children: SmallVec<[Id; 4]>,
}

impl Sym {
    fn hash_label(&self, h: &mut impl Hasher) {
        self.op.hash(h);
        self.children.len().hash(h);
    }
}

impl ENode for Sym {
    fn children(&self) -> &[Id] {
        &self.children
    }

    fn children_mut(&mut self) -> &mut [Id] {
        &mut self.children
    }

    fn hash_cons(&self) -> u64 {
        let mut h = FxHasher::default();
        self.hash_label(&mut h);
        self.children.hash(&mut h);
        h.finish()
    }

    fn op_key(&self) -> u64 {
        let mut h = FxHasher::default();
        self.hash_label(&mut h);
        h.finish()
    }

    fn label_hash(&self) -> u64 {
        self.op_key()
    }

    fn matches(&self, other: &Self) -> bool {
        self.op == other.op && self.children.len() == other.children.len()
    }
}

/// One rule's variables, atoms and head as it is built out of the patterns.
/// Holes take the first variables, so a hole's number is its variable.
struct Build<'a> {
    vars: u32,
    atoms: Vec<Atom<Sym>>,
    head: Vec<HeadOp<Sym>>,
    /// Each sub-pattern of the left-hand side and the variable its class binds.
    matched: Vec<(&'a Pattern, u32)>,
}

impl<'a> Build<'a> {
    fn var(&mut self) -> u32 {
        self.vars += 1;
        self.vars - 1
    }

    fn node(op: u32, children: &[u32]) -> Sym {
        Sym {
            op,
            children: children.iter().map(|&var| Id::from_raw(var)).collect(),
        }
    }

    /// The left-hand side: one atom per operator.
    fn left(&mut self, pattern: &'a Pattern) -> u32 {
        match pattern {
            Pattern::Var(var) => *var,
            Pattern::Node(op, children) => {
                let class = self.var();
                let args: SmallVec<[u32; 4]> = children.iter().map(|c| self.left(c)).collect();
                self.atoms.push(Atom::Node {
                    template: Self::node(*op, &args),
                    args,
                    class,
                    row: None,
                });
                self.matched.push((pattern, class));
                class
            }
        }
    }

    /// The right-hand side: an insert per operator. A sub-pattern the left-hand
    /// side already matched is the class it bound, as a rule of the compiler
    /// names it, rather than a second copy to look up.
    fn right(&mut self, pattern: &Pattern) -> u32 {
        if let Some(&(_, class)) = self.matched.iter().find(|(seen, _)| *seen == pattern) {
            return class;
        }
        match pattern {
            Pattern::Var(var) => *var,
            Pattern::Node(op, children) => {
                let args: SmallVec<[u32; 4]> = children.iter().map(|c| self.right(c)).collect();
                let into = self.var();
                self.head.push(HeadOp::Insert {
                    label: LabelFill::plain(Self::node(*op, &args)),
                    args,
                    into,
                });
                into
            }
        }
    }
}

fn build_rule(spec: &RuleSpec) -> Rule<Sym> {
    let mut build = Build {
        vars: spec.lhs.vars().max(spec.rhs.vars()),
        atoms: Vec::new(),
        head: Vec::new(),
        matched: Vec::new(),
    };
    let root = build.left(&spec.lhs);
    // What the left-hand side binds is the query's; the rest is the head's.
    let vars = build.vars;
    let replacement = build.right(&spec.rhs);
    build.head.push(HeadOp::Union(root, replacement));
    Rule {
        name: spec.name.clone(),
        plan: Plan::compile(Query::tree(vars, root, build.atoms)),
        head: build.head,
        head_vars: build.vars - vars,
        post_saturation: false,
    }
}

fn build_rules(workload: &Workload) -> Vec<Rule<Sym>> {
    workload.rules.iter().map(build_rule).collect()
}

fn seed(workload: &Workload) -> Engine<Sym> {
    let mut g = Engine::new();
    let mut classes: Vec<Option<Id>> = vec![None; workload.classes];
    for seed in &workload.seeds {
        let id = g.add(Sym {
            op: seed.symbol,
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

fn saturate(mut g: Engine<Sym>, rules: &[Rule<Sym>], iters: usize) -> Engine<Sym> {
    g.saturate_rules(rules, &NoExterns, iters, NODE_LIMIT);
    g
}

fn ematch_all(rules: &[Rule<Sym>], g: &Engine<Sym>) -> usize {
    let mut total = 0usize;
    for rule in rules {
        let roots = rule.plan.roots(g);
        total += black_box(rule.plan.search(g, roots, &|_, _| true, false, &NoExterns)).len();
    }
    total
}

/// Print how each workload grows, one line per iteration, under
/// `TIR_WORKLOAD_STATS`. The three benchmarks print the same table, which is
/// how the engines are checked to be doing the same work.
fn print_stats(workloads: &[Workload]) {
    if std::env::var_os("TIR_WORKLOAD_STATS").is_none() {
        return;
    }
    for workload in workloads {
        let rules = build_rules(workload);
        for iters in 0..=workload.iters {
            let g = saturate(seed(workload), &rules, iters);
            eprintln!(
                "stats engine=tir workload={} iters={iters} classes={} nodes={}",
                workload.name,
                g.num_classes(),
                g.total_size()
            );
        }
    }
}

fn benches(c: &mut Criterion) {
    let workloads = shared::workloads();
    print_stats(&workloads);
    let mut group = c.benchmark_group("tir/seed");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            b.iter(|| seed(workload));
        });
    }
    group.finish();
    let mut group = c.benchmark_group("tir/saturate");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            let rules = build_rules(workload);
            b.iter_batched(
                || seed(workload),
                |g| saturate(g, &rules, workload.iters),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
    let mut group = c.benchmark_group("tir/ematch");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            let rules = build_rules(workload);
            let g = saturate(seed(workload), &rules, workload.iters);
            b.iter(|| ematch_all(&rules, &g));
        });
    }
    group.finish();
    let mut group = c.benchmark_group("tir/extract");
    for workload in &workloads {
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            let rules = build_rules(workload);
            let g = saturate(seed(workload), &rules, workload.iters);
            b.iter(|| black_box(g.extract_best(|_, _| 1)));
        });
    }
    group.finish();
}

criterion_group!(all, benches);
criterion_main!(all);
