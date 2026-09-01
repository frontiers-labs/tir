//! egg counterpart of the `egraph` benchmark: the same symbolic-math workload
//! (identical language, [`shared::RULES`], [`shared::SEED_EXPRS`] and iteration
//! budget) run on egg, so the two engines can be compared head to head.
//!
//! To keep the comparison fair the rules are pure structural rewriting: no
//! `ConstantFold` analysis (`Analysis = ()`), and side conditions inspect e-nodes
//! exactly as the TIR bench does. The `SimpleScheduler` applies every match each
//! iteration, matching TIR's `saturate`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use egg::{
    ConditionalApplier, CostFunction, EGraph, Extractor, Id, Language, Pattern, Rewrite, Runner,
    SimpleScheduler, Subst, Symbol, Var, define_language,
};

#[path = "math_shared.rs"]
mod shared;
use shared::{Cond, PRE_SAT_ITERS, RULES, RuleSpec, SAT_ITERS, SEED_EXPRS};

define_language! {
    enum Math {
        "d" = Diff([Id; 2]),
        "i" = Integral([Id; 2]),
        "+" = Add([Id; 2]),
        "-" = Sub([Id; 2]),
        "*" = Mul([Id; 2]),
        "/" = Div([Id; 2]),
        "pow" = Pow([Id; 2]),
        "ln" = Ln(Id),
        "sqrt" = Sqrt(Id),
        "sin" = Sin(Id),
        "cos" = Cos(Id),
        Constant(i64),
        Symbol(Symbol),
    }
}

struct MathCost;
impl CostFunction<Math> for MathCost {
    type Cost = usize;
    fn cost<C: FnMut(Id) -> usize>(&mut self, enode: &Math, mut costs: C) -> usize {
        let op = match enode {
            Math::Diff(..) | Math::Integral(..) => 100,
            _ => 1,
        };
        enode.fold(op, |sum, i| sum + costs(i))
    }
}

fn var(v: &str) -> Var {
    v.parse().unwrap()
}

fn eval_cond(c: &Cond, egraph: &EGraph<Math, ()>, subst: &Subst) -> bool {
    match *c {
        Cond::NotZero(v) => !egraph[subst[var(v)]]
            .nodes
            .iter()
            .any(|n| matches!(n, Math::Constant(0))),
        Cond::Sym(v) => egraph[subst[var(v)]]
            .nodes
            .iter()
            .any(|n| matches!(n, Math::Symbol(_))),
        Cond::Const(v) => egraph[subst[var(v)]]
            .nodes
            .iter()
            .any(|n| matches!(n, Math::Constant(_))),
        Cond::ConstOrDistinct(cv, xv) => {
            egraph.find(subst[var(cv)]) != egraph.find(subst[var(xv)])
                && egraph[subst[var(cv)]]
                    .nodes
                    .iter()
                    .any(|n| matches!(n, Math::Constant(_) | Math::Symbol(_)))
        }
    }
}

fn build_rule(spec: &RuleSpec) -> Rewrite<Math, ()> {
    let searcher: Pattern<Math> = spec.lhs.parse().unwrap();
    let applier: Pattern<Math> = spec.rhs.parse().unwrap();
    if spec.conds.is_empty() {
        Rewrite::new(spec.name, searcher, applier).unwrap()
    } else {
        let conds = spec.conds;
        let condition = move |egraph: &mut EGraph<Math, ()>, _id: Id, subst: &Subst| {
            conds.iter().all(|c| eval_cond(c, egraph, subst))
        };
        let applier = ConditionalApplier { condition, applier };
        Rewrite::new(spec.name, searcher, applier).unwrap()
    }
}

fn build_rules() -> Vec<Rewrite<Math, ()>> {
    RULES.iter().map(build_rule).collect()
}

fn seed_runner(iters: usize) -> Runner<Math, ()> {
    let mut runner = Runner::default()
        .with_iter_limit(iters)
        .with_node_limit(1_000_000)
        .with_time_limit(Duration::from_secs(60))
        .with_scheduler(SimpleScheduler);
    for s in SEED_EXPRS {
        runner = runner.with_expr(&s.parse().unwrap());
    }
    runner
}

fn bench_saturate(c: &mut Criterion) {
    let rules = build_rules();
    let mut group = c.benchmark_group("egg_math/saturate");
    for &iters in SAT_ITERS {
        group.bench_with_input(BenchmarkId::from_parameter(iters), &iters, |b, &iters| {
            b.iter_batched(
                || seed_runner(iters),
                |runner| runner.run(&rules),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_ematch(c: &mut Criterion) {
    let rules = build_rules();
    let runner = seed_runner(PRE_SAT_ITERS).run(&rules);
    let egraph = &runner.egraph;
    let mut group = c.benchmark_group("egg_math/ematch");
    group.bench_function("all_rules", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for rule in &rules {
                for m in rule.search(egraph) {
                    total += black_box(m.substs.len());
                }
            }
            total
        });
    });
    group.finish();
}

fn bench_extract(c: &mut Criterion) {
    let rules = build_rules();
    let runner = seed_runner(PRE_SAT_ITERS).run(&rules);
    let egraph = &runner.egraph;
    let roots = runner.roots.clone();
    let mut group = c.benchmark_group("egg_math/extract");
    group.bench_function("best", |b| {
        b.iter(|| {
            let extractor = Extractor::new(egraph, MathCost);
            let mut total = 0usize;
            for &root in &roots {
                total += black_box(extractor.find_best_cost(root));
            }
            total
        });
    });
    group.finish();
}

criterion_group!(benches, bench_saturate, bench_ematch, bench_extract);
criterion_main!(benches);
