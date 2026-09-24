//! egg counterpart of the `egraph` benchmark: the same symbolic-math workload
//! (identical language, [`shared::RULES`], [`shared::SEED_EXPRS`] and iteration
//! budget) run on egg, so the two engines can be compared head to head.
//!
//! To keep the comparison fair the rules are pure structural rewriting: no
//! `ConstantFold` analysis (`Analysis = ()`), and side conditions inspect e-nodes
//! exactly as the TIR bench does. The `SimpleScheduler` applies every match each
//! iteration, matching TIR's `saturate`.

#[macro_use]
#[path = "../../../benchmarks/functions.rs"]
pub mod functions;

use std::hint::black_box;
use std::time::Duration;

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

fn saturate(runner: Runner<Math, ()>, rules: &[Rewrite<Math, ()>]) -> Runner<Math, ()> {
    runner.run(rules)
}

fn pre_saturated() -> (Vec<Rewrite<Math, ()>>, Runner<Math, ()>) {
    let rules = build_rules();
    let runner = saturate(seed_runner(PRE_SAT_ITERS), &rules);
    (rules, runner)
}

fn ematch_all(rules: &[Rewrite<Math, ()>], egraph: &EGraph<Math, ()>) -> usize {
    let mut total = 0usize;
    for rule in rules {
        for matched in rule.search(egraph) {
            total += black_box(matched.substs.len());
        }
    }
    total
}

fn extract_all(egraph: &EGraph<Math, ()>, roots: &[Id]) -> usize {
    let extractor = Extractor::new(egraph, MathCost);
    let mut total = 0usize;
    for &root in roots {
        total += black_box(extractor.find_best_cost(root));
    }
    total
}

fn bench_saturate(b: &mut functions::Bencher<'_, '_>, iters: usize) {
    let rules = build_rules();
    b.iter_batched(|| seed_runner(iters), |runner| saturate(runner, &rules));
}

benchmarks! {
    compiler = "egg";
    saturate_1("egg_math/saturate/1") |b| { bench_saturate(b, SAT_ITERS[0]); }
    saturate_2("egg_math/saturate/2") |b| { bench_saturate(b, SAT_ITERS[1]); }
    saturate_3("egg_math/saturate/3") |b| { bench_saturate(b, SAT_ITERS[2]); }
    bench_ematch("egg_math/ematch/all_rules") |b| {
        let (rules, runner) = pre_saturated();
        b.iter(|| ematch_all(&rules, &runner.egraph));
    }
    bench_extract("egg_math/extract/best") |b| {
        let (_, runner) = pre_saturated();
        let roots = runner.roots.clone();
        b.iter(|| extract_all(&runner.egraph, &roots));
    }
}
