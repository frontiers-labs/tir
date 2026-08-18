//! A tiny arithmetic language shared by the e-graph, pattern, and rewrite tests.

use tir_adt::{APFloat, APInt};

use tir_symbolic::egraph::{EGraph, ENode, Id, Pattern, Rewrite, Rhs, Var};

#[derive(Debug, Clone)]
pub(crate) enum Math {
    Num(i64),
    FNum(APFloat),
    Sym(u32),
    Neg([Id; 1]),
    Add([Id; 2]),
    /// A never-shared effectful node: discriminant `kind`, one operand.
    Effect(u32, [Id; 1]),
}

impl ENode for Math {
    fn children(&self) -> &[Id] {
        match self {
            Math::Num(_) | Math::FNum(_) | Math::Sym(_) => &[],
            Math::Neg(c) | Math::Effect(_, c) => c,
            Math::Add(c) => c,
        }
    }

    fn children_mut(&mut self) -> &mut [Id] {
        match self {
            Math::Num(_) | Math::FNum(_) | Math::Sym(_) => &mut [],
            Math::Neg(c) | Math::Effect(_, c) => c,
            Math::Add(c) => c,
        }
    }

    // Buckets by operator only, so all `Num`s collide — exercises matches()+children disambiguation.
    fn hash_cons(&self) -> u64 {
        let mut hash = self.op_key();
        for child in self.children() {
            hash = hash.rotate_left(5) ^ child.index() as u64;
        }
        hash
    }

    fn op_key(&self) -> u64 {
        match self {
            Math::Num(_) => 1,
            Math::Sym(_) => 2,
            Math::Neg(_) => 3,
            Math::Add(_) => 4,
            Math::Effect(..) => 5,
            Math::FNum(_) => 6,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Math::Num(a), Math::Num(b)) => a == b,
            (Math::FNum(a), Math::FNum(b)) => a == b,
            (Math::Sym(a), Math::Sym(b)) => a == b,
            (Math::Neg(_), Math::Neg(_)) => true,
            (Math::Add(_), Math::Add(_)) => true,
            (Math::Effect(a, _), Math::Effect(b, _)) => a == b,
            _ => false,
        }
    }

    fn is_unique(&self) -> bool {
        matches!(self, Math::Effect(..))
    }

    fn from_int(value: APInt) -> Option<Self> {
        Some(Math::Num(value.to_i64()))
    }

    fn from_float(value: APFloat) -> Option<Self> {
        Some(Math::FNum(value))
    }
}

pub(crate) fn num(g: &mut EGraph<Math>, n: i64) -> Id {
    g.add(Math::Num(n))
}
pub(crate) fn fnum(g: &mut EGraph<Math>, v: f64) -> Id {
    g.add(Math::FNum(APFloat::from_f64(v)))
}
pub(crate) fn sym(g: &mut EGraph<Math>, s: u32) -> Id {
    g.add(Math::Sym(s))
}
pub(crate) fn neg(g: &mut EGraph<Math>, a: Id) -> Id {
    g.add(Math::Neg([a]))
}
pub(crate) fn add(g: &mut EGraph<Math>, a: Id, b: Id) -> Id {
    g.add(Math::Add([a, b]))
}

/// `add(x, y) => add(y, x)`.
pub(crate) fn comm_rule() -> Rewrite<Math, &'static str> {
    let mut lhs: Pattern<Math, &'static str> = Pattern::new();
    let x = lhs.var(Var::Symbol("x"));
    let y = lhs.var(Var::Symbol("y"));
    lhs.add(Math::Add([x, y]));

    let mut rhs: Pattern<Math, &'static str> = Pattern::new();
    let rx = rhs.var(Var::Symbol("x"));
    let ry = rhs.var(Var::Symbol("y"));
    rhs.add(Math::Add([ry, rx]));

    Rewrite::new("add-comm", lhs, Rhs::Pattern(rhs))
}

/// `add(x, 0) => x`.
pub(crate) fn add_zero_rule() -> Rewrite<Math, &'static str> {
    let mut lhs: Pattern<Math, &'static str> = Pattern::new();
    let x = lhs.var(Var::Symbol("x"));
    let zero = lhs.var(Var::Int(APInt::from_i64(0)));
    lhs.add(Math::Add([x, zero]));

    let mut rhs: Pattern<Math, &'static str> = Pattern::new();
    rhs.var(Var::Symbol("x"));

    Rewrite::new("add-zero", lhs, Rhs::Pattern(rhs))
}
