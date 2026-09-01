//! Shared workload definition for the two symbolic-math e-graph benchmarks
//! (`egraph` = TIR, `egg_math` = egg). Both binaries `#[path]`-include this file
//! and build engine-specific rule objects from the *same* specs, so they saturate
//! the same language, ruleset, seed expressions and iteration budget. Keep this
//! file free of engine types — only plain data.
//!
//! Modelled on egg's `tests/math.rs`. Constant folding (egg's `ConstantFold`
//! analysis) is deliberately omitted: this is a purely structural rewriting
//! workload, identical on both engines.
#![allow(dead_code)]

/// Side condition on a rule, evaluated by inspecting the bound class's e-nodes
/// (never analysis data, which neither engine carries here).
#[derive(Clone, Copy)]
pub enum Cond {
    /// The variable's class holds no `Constant(0)` e-node.
    NotZero(&'static str),
    /// The variable's class holds a `Symbol` e-node.
    Sym(&'static str),
    /// The variable's class holds a `Constant` e-node.
    Const(&'static str),
    /// `c` is a different class than `x`, and `c` is a constant or a symbol.
    ConstOrDistinct(&'static str, &'static str),
}

pub struct RuleSpec {
    pub name: &'static str,
    pub lhs: &'static str,
    pub rhs: &'static str,
    pub conds: &'static [Cond],
}

/// The full egg `math.rs` ruleset.
pub const RULES: &[RuleSpec] = &[
    RuleSpec {
        name: "comm-add",
        lhs: "(+ ?a ?b)",
        rhs: "(+ ?b ?a)",
        conds: &[],
    },
    RuleSpec {
        name: "comm-mul",
        lhs: "(* ?a ?b)",
        rhs: "(* ?b ?a)",
        conds: &[],
    },
    RuleSpec {
        name: "assoc-add",
        lhs: "(+ ?a (+ ?b ?c))",
        rhs: "(+ (+ ?a ?b) ?c)",
        conds: &[],
    },
    RuleSpec {
        name: "assoc-mul",
        lhs: "(* ?a (* ?b ?c))",
        rhs: "(* (* ?a ?b) ?c)",
        conds: &[],
    },
    RuleSpec {
        name: "sub-canon",
        lhs: "(- ?a ?b)",
        rhs: "(+ ?a (* -1 ?b))",
        conds: &[],
    },
    RuleSpec {
        name: "div-canon",
        lhs: "(/ ?a ?b)",
        rhs: "(* ?a (pow ?b -1))",
        conds: &[Cond::NotZero("?b")],
    },
    RuleSpec {
        name: "zero-add",
        lhs: "(+ ?a 0)",
        rhs: "?a",
        conds: &[],
    },
    RuleSpec {
        name: "zero-mul",
        lhs: "(* ?a 0)",
        rhs: "0",
        conds: &[],
    },
    RuleSpec {
        name: "one-mul",
        lhs: "(* ?a 1)",
        rhs: "?a",
        conds: &[],
    },
    RuleSpec {
        name: "add-zero",
        lhs: "?a",
        rhs: "(+ ?a 0)",
        conds: &[],
    },
    RuleSpec {
        name: "mul-one",
        lhs: "?a",
        rhs: "(* ?a 1)",
        conds: &[],
    },
    RuleSpec {
        name: "cancel-sub",
        lhs: "(- ?a ?a)",
        rhs: "0",
        conds: &[],
    },
    RuleSpec {
        name: "cancel-div",
        lhs: "(/ ?a ?a)",
        rhs: "1",
        conds: &[Cond::NotZero("?a")],
    },
    RuleSpec {
        name: "distribute",
        lhs: "(* ?a (+ ?b ?c))",
        rhs: "(+ (* ?a ?b) (* ?a ?c))",
        conds: &[],
    },
    RuleSpec {
        name: "factor",
        lhs: "(+ (* ?a ?b) (* ?a ?c))",
        rhs: "(* ?a (+ ?b ?c))",
        conds: &[],
    },
    RuleSpec {
        name: "pow-mul",
        lhs: "(* (pow ?a ?b) (pow ?a ?c))",
        rhs: "(pow ?a (+ ?b ?c))",
        conds: &[],
    },
    RuleSpec {
        name: "pow0",
        lhs: "(pow ?x 0)",
        rhs: "1",
        conds: &[Cond::NotZero("?x")],
    },
    RuleSpec {
        name: "pow1",
        lhs: "(pow ?x 1)",
        rhs: "?x",
        conds: &[],
    },
    RuleSpec {
        name: "pow2",
        lhs: "(pow ?x 2)",
        rhs: "(* ?x ?x)",
        conds: &[],
    },
    RuleSpec {
        name: "pow-recip",
        lhs: "(pow ?x -1)",
        rhs: "(/ 1 ?x)",
        conds: &[Cond::NotZero("?x")],
    },
    RuleSpec {
        name: "recip-mul-div",
        lhs: "(* ?x (/ 1 ?x))",
        rhs: "1",
        conds: &[Cond::NotZero("?x")],
    },
    RuleSpec {
        name: "d-variable",
        lhs: "(d ?x ?x)",
        rhs: "1",
        conds: &[Cond::Sym("?x")],
    },
    RuleSpec {
        name: "d-constant",
        lhs: "(d ?x ?c)",
        rhs: "0",
        conds: &[Cond::Sym("?x"), Cond::ConstOrDistinct("?c", "?x")],
    },
    RuleSpec {
        name: "d-add",
        lhs: "(d ?x (+ ?a ?b))",
        rhs: "(+ (d ?x ?a) (d ?x ?b))",
        conds: &[],
    },
    RuleSpec {
        name: "d-mul",
        lhs: "(d ?x (* ?a ?b))",
        rhs: "(+ (* ?a (d ?x ?b)) (* ?b (d ?x ?a)))",
        conds: &[],
    },
    RuleSpec {
        name: "d-sin",
        lhs: "(d ?x (sin ?x))",
        rhs: "(cos ?x)",
        conds: &[],
    },
    RuleSpec {
        name: "d-cos",
        lhs: "(d ?x (cos ?x))",
        rhs: "(* -1 (sin ?x))",
        conds: &[],
    },
    RuleSpec {
        name: "d-ln",
        lhs: "(d ?x (ln ?x))",
        rhs: "(/ 1 ?x)",
        conds: &[Cond::NotZero("?x")],
    },
    RuleSpec {
        name: "d-power",
        lhs: "(d ?x (pow ?f ?g))",
        rhs: "(* (pow ?f ?g) (+ (* (d ?x ?f) (/ ?g ?f)) (* (d ?x ?g) (ln ?f))))",
        conds: &[Cond::NotZero("?f"), Cond::NotZero("?g")],
    },
    RuleSpec {
        name: "i-one",
        lhs: "(i 1 ?x)",
        rhs: "?x",
        conds: &[],
    },
    RuleSpec {
        name: "i-power-const",
        lhs: "(i (pow ?x ?c) ?x)",
        rhs: "(/ (pow ?x (+ ?c 1)) (+ ?c 1))",
        conds: &[Cond::Const("?c")],
    },
    RuleSpec {
        name: "i-cos",
        lhs: "(i (cos ?x) ?x)",
        rhs: "(sin ?x)",
        conds: &[],
    },
    RuleSpec {
        name: "i-sin",
        lhs: "(i (sin ?x) ?x)",
        rhs: "(* -1 (cos ?x))",
        conds: &[],
    },
    RuleSpec {
        name: "i-sum",
        lhs: "(i (+ ?f ?g) ?x)",
        rhs: "(+ (i ?f ?x) (i ?g ?x))",
        conds: &[],
    },
    RuleSpec {
        name: "i-dif",
        lhs: "(i (- ?f ?g) ?x)",
        rhs: "(- (i ?f ?x) (i ?g ?x))",
        conds: &[],
    },
    RuleSpec {
        name: "i-parts",
        lhs: "(i (* ?a ?b) ?x)",
        rhs: "(- (* ?a (i ?b ?x)) (i (* (d ?x ?a) (i ?b ?x)) ?x))",
        conds: &[],
    },
];

/// Seed expressions (egg's `math_ematching_bench` inputs).
pub const SEED_EXPRS: &[&str] = &[
    "(i (ln x) x)",
    "(i (+ x (cos x)) x)",
    "(i (* (cos x) x) x)",
    "(d x (+ 1 (* 2 x)))",
    "(d x (- (pow x 3) (* 7 (pow x 2))))",
    "(+ (* y (+ x y)) (- (+ x 2) (+ x x)))",
    "(/ 1 (- (/ (+ 1 (sqrt five)) 2) (/ (- 1 (sqrt five)) 2)))",
];

/// Iteration budgets for the `saturate` benchmark.
pub const SAT_ITERS: &[usize] = &[1, 2, 3];

/// Iterations to pre-saturate before the `ematch` / `extract` benchmarks.
pub const PRE_SAT_ITERS: usize = 2;
