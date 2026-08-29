//! Symbolic-math equality-saturation benchmark for `tir-symbolic`, vs egg's `tests/math.rs`
//! on the same [`shared::RULES`]/[`shared::SEED_EXPRS`]. Names intern to `u32` (see [`intern`])
//! so the comparison measures e-matching, not string handling — matching egg's `Copy` names.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::sync::{Mutex, OnceLock};

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tir_adt::{APInt, FxHasher};
use tir_symbolic::egraph::{EGraph, ENode, Id, Pattern, Rewrite, Rhs, Substitution, Var};

#[path = "math_shared.rs"]
mod shared;
use shared::{Cond, PRE_SAT_ITERS, RULES, RuleSpec, SAT_ITERS, SEED_EXPRS};

const NODE_LIMIT: usize = 1_000_000;

/// Intern a name to a stable `u32`, mirroring egg's global symbol interner.
fn intern(name: &str) -> u32 {
    static TABLE: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    let table = TABLE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table.lock().unwrap();
    let next = table.len() as u32;
    *table.entry(name.to_string()).or_insert(next)
}

/// The math language label; constants/symbols carry their value/name for label-equality matching.
#[derive(Clone, Debug)]
enum Math {
    Diff([Id; 2]),
    Integral([Id; 2]),
    Add([Id; 2]),
    Sub([Id; 2]),
    Mul([Id; 2]),
    Div([Id; 2]),
    Pow([Id; 2]),
    Ln([Id; 1]),
    Sqrt([Id; 1]),
    Sin([Id; 1]),
    Cos([Id; 1]),
    Constant(i64),
    Symbol(u32),
}

// Shared match body for the children/children_mut accessors; `$empty` is the leaf slice.
macro_rules! math_children {
    ($val:expr, $empty:expr) => {
        match $val {
            Math::Diff(a)
            | Math::Integral(a)
            | Math::Add(a)
            | Math::Sub(a)
            | Math::Mul(a)
            | Math::Div(a)
            | Math::Pow(a) => a,
            Math::Ln(a) | Math::Sqrt(a) | Math::Sin(a) | Math::Cos(a) => a,
            Math::Constant(_) | Math::Symbol(_) => $empty,
        }
    };
}

impl ENode for Math {
    fn children(&self) -> &[Id] {
        math_children!(self, &[])
    }

    fn children_mut(&mut self) -> &mut [Id] {
        math_children!(self, &mut [])
    }

    fn hash_cons(&self) -> u64 {
        let mut h = FxHasher::default();
        hash_label(self, &mut h);
        self.children().hash(&mut h);
        h.finish()
    }

    fn op_key(&self) -> u64 {
        let mut h = FxHasher::default();
        hash_label(self, &mut h);
        h.finish()
    }

    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Math::Constant(a), Math::Constant(b)) => a == b,
            (Math::Symbol(a), Math::Symbol(b)) => a == b,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }

    fn from_int(value: APInt) -> Option<Self> {
        Some(Math::Constant(value.to_i64()))
    }
}

fn hash_label(node: &Math, h: &mut impl Hasher) {
    std::mem::discriminant(node).hash(h);
    match node {
        Math::Constant(n) => n.hash(h),
        Math::Symbol(s) => s.hash(h),
        _ => {}
    }
}

/// Build the operator label for `head` over already-built `children`.
fn make_node(head: &str, children: &[Id]) -> Math {
    let two = || [children[0], children[1]];
    let one = || [children[0]];
    match head {
        "+" => Math::Add(two()),
        "-" => Math::Sub(two()),
        "*" => Math::Mul(two()),
        "/" => Math::Div(two()),
        "pow" => Math::Pow(two()),
        "ln" => Math::Ln(one()),
        "sqrt" => Math::Sqrt(one()),
        "sin" => Math::Sin(one()),
        "cos" => Math::Cos(one()),
        "d" => Math::Diff(two()),
        "i" => Math::Integral(two()),
        other => panic!("unknown operator {other}"),
    }
}

enum Sexp {
    Atom(String),
    List(Vec<Sexp>),
}

fn tokenize(s: &str) -> Vec<String> {
    s.replace('(', " ( ")
        .replace(')', " ) ")
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn parse_tokens(toks: &[String], pos: &mut usize) -> Sexp {
    let tok = toks[*pos].clone();
    *pos += 1;
    if tok == "(" {
        let mut items = Vec::new();
        while toks[*pos] != ")" {
            items.push(parse_tokens(toks, pos));
        }
        *pos += 1;
        Sexp::List(items)
    } else {
        Sexp::Atom(tok)
    }
}

fn parse_sexp(s: &str) -> Sexp {
    let toks = tokenize(s);
    let mut pos = 0;
    parse_tokens(&toks, &mut pos)
}

fn is_var(a: &str) -> bool {
    a.starts_with('?')
}

fn atom_str(e: &Sexp) -> &str {
    match e {
        Sexp::Atom(a) => a,
        Sexp::List(_) => panic!("expected operator atom"),
    }
}

fn add_expr(g: &mut EGraph<Math>, e: &Sexp) -> Id {
    match e {
        Sexp::Atom(a) => {
            if let Ok(n) = a.parse::<i64>() {
                g.add(Math::Constant(n))
            } else {
                g.add(Math::Symbol(intern(a)))
            }
        }
        Sexp::List(items) => {
            let children: Vec<Id> = items[1..].iter().map(|c| add_expr(g, c)).collect();
            g.add(make_node(atom_str(&items[0]), &children))
        }
    }
}

/// Build a pattern from `e`, reusing one hole per `?var` name.
fn build_pattern(e: &Sexp) -> Pattern<Math, u32> {
    let mut p = Pattern::new();
    let mut vars = HashMap::new();
    let root = add_pat(&mut p, e, &mut vars);
    p.set_root(root);
    p
}

fn add_pat(p: &mut Pattern<Math, u32>, e: &Sexp, vars: &mut HashMap<String, Id>) -> Id {
    match e {
        Sexp::Atom(a) => {
            if is_var(a) {
                *vars
                    .entry(a.clone())
                    .or_insert_with(|| p.var(Var::Symbol(intern(a))))
            } else if let Ok(n) = a.parse::<i64>() {
                p.var(Var::Int(APInt::from_i64(n)))
            } else {
                p.add(Math::Symbol(intern(a)))
            }
        }
        Sexp::List(items) => {
            let children: Vec<Id> = items[1..].iter().map(|c| add_pat(p, c, vars)).collect();
            p.add(make_node(atom_str(&items[0]), &children))
        }
    }
}

fn class_has(g: &EGraph<Math>, class: Id, pred: impl Fn(&Math) -> bool) -> bool {
    g.nodes(class).any(pred)
}

fn var_class(subst: &Substitution<u32>, v: &str) -> Id {
    subst
        .get(&Var::Symbol(intern(v)))
        .expect("condition variable is bound by the searcher")
}

fn eval_cond(c: &Cond, g: &EGraph<Math>, subst: &Substitution<u32>) -> bool {
    match *c {
        Cond::NotZero(v) => !class_has(g, var_class(subst, v), |n| matches!(n, Math::Constant(0))),
        Cond::Sym(v) => class_has(g, var_class(subst, v), |n| matches!(n, Math::Symbol(_))),
        Cond::Const(v) => class_has(g, var_class(subst, v), |n| matches!(n, Math::Constant(_))),
        Cond::ConstOrDistinct(cv, xv) => {
            let cc = var_class(subst, cv);
            g.find(cc) != g.find(var_class(subst, xv))
                && (class_has(g, cc, |n| matches!(n, Math::Constant(_)))
                    || class_has(g, cc, |n| matches!(n, Math::Symbol(_))))
        }
    }
}

fn build_rule(spec: &RuleSpec) -> Rewrite<Math, u32> {
    let lhs = build_pattern(&parse_sexp(spec.lhs));
    let rhs = build_pattern(&parse_sexp(spec.rhs));
    let conds = spec.conds;
    let apply = Box::new(
        move |g: &mut EGraph<Math>, subst: &Substitution<u32>, root: Id| {
            if !conds.iter().all(|c| eval_cond(c, g, subst)) {
                return;
            }
            let new = rhs.instantiate(g, subst);
            g.union(root, new);
        },
    );
    Rewrite::new(spec.name, lhs, Rhs::Apply(apply))
}

fn build_rules() -> Vec<Rewrite<Math, u32>> {
    RULES.iter().map(build_rule).collect()
}

fn seed_all() -> EGraph<Math> {
    let mut g = EGraph::new();
    for s in SEED_EXPRS {
        add_expr(&mut g, &parse_sexp(s));
    }
    g
}

fn pre_saturated() -> (Vec<Rewrite<Math, u32>>, EGraph<Math>) {
    let rules = build_rules();
    let mut g = seed_all();
    g.saturate(&rules, PRE_SAT_ITERS, NODE_LIMIT);
    (rules, g)
}

fn extract_cost(node: &Math) -> u64 {
    match node {
        Math::Diff(_) | Math::Integral(_) => 100,
        _ => 1,
    }
}

fn bench_saturate(c: &mut Criterion) {
    let rules = build_rules();
    let mut group = c.benchmark_group("tir_math/saturate");
    for &iters in SAT_ITERS {
        group.bench_with_input(BenchmarkId::from_parameter(iters), &iters, |b, &iters| {
            b.iter_batched(
                seed_all,
                |mut g| {
                    g.saturate(&rules, iters, NODE_LIMIT);
                    g
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// The pre-semi-naive driver: every round searches every rule over the whole
/// graph. Kept here as the baseline [`EGraph::saturate`]'s delta rounds are
/// measured against.
fn saturate_naive(g: &mut EGraph<Math>, rules: &[Rewrite<Math, u32>], iters: usize) {
    for _ in 0..iters {
        let size = g.total_size();
        if size >= NODE_LIMIT {
            break;
        }
        let before = (g.num_classes(), size);
        let searched: Vec<_> = rules
            .iter()
            .map(|rule| (rule, rule.lhs.search(g)))
            .collect();
        for (rule, matches) in &searched {
            for m in matches {
                rule.apply_match(g, m);
            }
        }
        g.rebuild();
        if (g.num_classes(), g.total_size()) == before {
            break;
        }
    }
}

fn bench_saturate_naive(c: &mut Criterion) {
    let rules = build_rules();
    let mut group = c.benchmark_group("tir_math/saturate_naive");
    for &iters in SAT_ITERS {
        group.bench_with_input(BenchmarkId::from_parameter(iters), &iters, |b, &iters| {
            b.iter_batched(
                seed_all,
                |mut g| {
                    saturate_naive(&mut g, &rules, iters);
                    g
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_ematch(c: &mut Criterion) {
    let (rules, g) = pre_saturated();
    let mut group = c.benchmark_group("tir_math/ematch");
    group.bench_function("all_rules", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for rule in &rules {
                total += black_box(rule.lhs.search(&g)).len();
            }
            total
        });
    });
    group.finish();
}

fn bench_extract(c: &mut Criterion) {
    let (_, g) = pre_saturated();
    let mut group = c.benchmark_group("tir_math/extract");
    group.bench_function("best", |b| {
        b.iter(|| black_box(g.extract_best(|_, node| extract_cost(node))));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_saturate,
    bench_saturate_naive,
    bench_ematch,
    bench_extract
);
criterion_main!(benches);
