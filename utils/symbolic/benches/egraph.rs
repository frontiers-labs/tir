//! Symbolic-math equality-saturation benchmark for `tir-symbolic`, vs egg's `tests/math.rs`
//! on the same [`shared::RULES`]/[`shared::SEED_EXPRS`]. Names intern to `u32` (see [`intern`])
//! so the comparison measures e-matching, not string handling — matching egg's `Copy` names.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::sync::{Mutex, OnceLock};

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use smallvec::{SmallVec, smallvec};
use tir_adt::{APInt, FxHasher};
use tir_relational::{Atom, Guard, HeadOp, LabelFill, Match, Nested, NoExterns, Plan, Query, Rule};
use tir_symbolic::egraph::{EGraph, ENode, Id};

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

    fn constant(&self) -> Option<Self> {
        matches!(self, Math::Constant(_)).then(|| self.clone())
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

/// One rule's variables, atoms and head as it is built out of the s-expressions.
#[derive(Default)]
struct Build {
    vars: u32,
    atoms: Vec<Atom<Math>>,
    head: Vec<HeadOp<Math>>,
    holes: HashMap<String, u32>,
}

impl Build {
    fn var(&mut self) -> u32 {
        self.vars += 1;
        self.vars - 1
    }

    /// The left-hand side: one atom per operator, one variable per `?var`, and a
    /// constant operand as a literal the class must be known to be.
    fn left(&mut self, e: &Sexp) -> u32 {
        match e {
            Sexp::Atom(a) if is_var(a) => {
                if let Some(&var) = self.holes.get(a) {
                    return var;
                }
                let var = self.var();
                self.holes.insert(a.clone(), var);
                var
            }
            Sexp::Atom(a) => {
                let class = self.var();
                let atom = match a.parse::<i64>() {
                    Ok(n) => Atom::Literal {
                        value: Math::Constant(n),
                        class,
                    },
                    Err(_) => Atom::Node {
                        template: Math::Symbol(intern(a)),
                        args: SmallVec::new(),
                        class,
                        row: None,
                    },
                };
                self.atoms.push(atom);
                class
            }
            Sexp::List(items) => {
                let class = self.var();
                let children: Vec<u32> = items[1..].iter().map(|c| self.left(c)).collect();
                let ids: Vec<Id> = children.iter().map(|&var| Id::from_raw(var)).collect();
                self.atoms.push(Atom::Node {
                    template: make_node(atom_str(&items[0]), &ids),
                    args: children.iter().copied().collect(),
                    class,
                    row: None,
                });
                class
            }
        }
    }

    /// The right-hand side: an insert per operator, and the variable a `?var`
    /// already bound.
    fn right(&mut self, e: &Sexp) -> u32 {
        match e {
            Sexp::Atom(a) if is_var(a) => self.holes[a],
            Sexp::Atom(a) => {
                let into = self.var();
                let template = match a.parse::<i64>() {
                    Ok(n) => Math::Constant(n),
                    Err(_) => Math::Symbol(intern(a)),
                };
                self.head.push(HeadOp::Insert {
                    label: LabelFill::plain(template),
                    args: SmallVec::new(),
                    into,
                });
                into
            }
            Sexp::List(items) => {
                let children: Vec<u32> = items[1..].iter().map(|c| self.right(c)).collect();
                let ids: Vec<Id> = children.iter().map(|&var| Id::from_raw(var)).collect();
                let into = self.var();
                self.head.push(HeadOp::Insert {
                    label: LabelFill::plain(make_node(atom_str(&items[0]), &ids)),
                    args: children.iter().copied().collect(),
                    into,
                });
                into
            }
        }
    }
}

/// A rule's side conditions as negated conjunctions and atoms. `ConstOrDistinct`
/// is a disjunction, so it is two rules rather than one.
fn conditions(
    build: &mut Build,
    cond: &Cond,
    symbol: bool,
) -> (Vec<Atom<Math>>, Vec<Nested<Math>>, Vec<Guard>) {
    let mut atoms = Vec::new();
    let mut nots = Vec::new();
    let mut guards = Vec::new();
    let holds = |op: u64, key: u32| Atom::Holds { key, op };
    match *cond {
        Cond::NotZero(v) => nots.push(Nested {
            atoms: vec![Atom::Literal {
                value: Math::Constant(0),
                class: build.holes[v],
            }],
            guards: Vec::new(),
        }),
        Cond::Sym(v) => atoms.push(holds(Math::Symbol(0).op_key(), build.holes[v])),
        Cond::Const(v) => atoms.push(Atom::Fact {
            column: tir_relational::ColumnId::Const,
            key: build.holes[v],
            value: 0,
        }),
        Cond::ConstOrDistinct(cv, xv) => {
            let (c, x) = (build.holes[cv], build.holes[xv]);
            guards.push(Guard::Distinct(smallvec![(c, x)]));
            atoms.push(if symbol {
                holds(Math::Symbol(0).op_key(), c)
            } else {
                Atom::Fact {
                    column: tir_relational::ColumnId::Const,
                    key: c,
                    value: 0,
                }
            });
        }
    }
    (atoms, nots, guards)
}

fn build_rule(spec: &RuleSpec, symbol: bool) -> Rule<Math> {
    let mut build = Build::default();
    let root = build.left(&parse_sexp(spec.lhs));
    let mut atoms = std::mem::take(&mut build.atoms);
    let mut nots = Vec::new();
    let mut guards = Vec::new();
    for cond in spec.conds {
        let (a, n, g) = conditions(&mut build, cond, symbol);
        atoms.extend(a);
        nots.extend(n);
        guards.extend(g);
    }
    let replacement = build.right(&parse_sexp(spec.rhs));
    let mut head = std::mem::take(&mut build.head);
    head.push(HeadOp::Union(root, replacement));
    Rule {
        name: spec.name.to_string(),
        plan: Plan::compile(Query {
            vars: build.vars,
            scalars: 1,
            root,
            atoms,
            guards,
            nots,
        }),
        head,
        post_saturation: false,
    }
}

fn build_rules() -> Vec<Rule<Math>> {
    RULES
        .iter()
        .flat_map(|spec| {
            let disjunctive = spec
                .conds
                .iter()
                .any(|cond| matches!(cond, Cond::ConstOrDistinct(..)));
            let mut rules = vec![build_rule(spec, false)];
            if disjunctive {
                rules.push(build_rule(spec, true));
            }
            rules
        })
        .collect()
}

fn seed_all() -> EGraph<Math> {
    let mut g = EGraph::new();
    for s in SEED_EXPRS {
        add_expr(&mut g, &parse_sexp(s));
    }
    g
}

fn pre_saturated() -> (Vec<Rule<Math>>, EGraph<Math>) {
    let rules = build_rules();
    let mut g = seed_all();
    g.saturate_rules(&rules, &NoExterns, PRE_SAT_ITERS, NODE_LIMIT);
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
                    g.saturate_rules(&rules, &NoExterns, iters, NODE_LIMIT);
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
fn saturate_naive(g: &mut EGraph<Math>, rules: &[Rule<Math>], iters: usize) {
    for _ in 0..iters {
        let size = g.total_size();
        if size >= NODE_LIMIT {
            break;
        }
        let before = (g.num_classes(), size);
        let searched: Vec<(&Rule<Math>, Vec<Match>)> = rules
            .iter()
            .map(|rule| {
                let roots = rule.plan.roots(g);
                let found = rule.plan.search(g, roots, &|_, _| true, false, &NoExterns);
                (rule, found)
            })
            .collect();
        for (rule, matches) in &searched {
            for m in matches {
                g.apply_head(&rule.head, m);
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
                let roots = rule.plan.roots(&g);
                total +=
                    black_box(rule.plan.search(&g, roots, &|_, _| true, false, &NoExterns)).len();
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
