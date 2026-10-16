//! A tiny arithmetic language shared by the e-graph, pattern, and rewrite tests.

use tir_adt::APFloat;

use tir_egraph::{Atom, HeadOp, LabelFill, NoExterns, Plan, Query, Rule};
use tir_egraph::{ClassId as Id, Engine, Label as ENode};

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
    type Names = ();
    type SortKey = (u8, u64);

    fn sort_key(&self, _: &()) -> Self::SortKey {
        match self {
            Math::Num(value) => (0, *value as u64),
            Math::FNum(value) => (1, value.to_f64().to_bits()),
            Math::Sym(name) => (2, u64::from(*name)),
            Math::Neg(_) => (3, 0),
            Math::Add(_) => (4, 0),
            Math::Effect(kind, _) => (5, u64::from(*kind)),
        }
    }

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

    fn constant(&self) -> Option<Self> {
        matches!(self, Math::Num(_) | Math::FNum(_)).then(|| self.clone())
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
}

pub(crate) fn num(g: &mut Engine<Math>, n: i64) -> Id {
    g.add(Math::Num(n)).class
}
pub(crate) fn fnum(g: &mut Engine<Math>, v: f64) -> Id {
    g.add(Math::FNum(APFloat::from_f64(v))).class
}
pub(crate) fn sym(g: &mut Engine<Math>, s: u32) -> Id {
    g.add(Math::Sym(s)).class
}
pub(crate) fn neg(g: &mut Engine<Math>, a: Id) -> Id {
    g.add(Math::Neg([a])).class
}
pub(crate) fn add(g: &mut Engine<Math>, a: Id, b: Id) -> Id {
    g.add(Math::Add([a, b])).class
}

pub(crate) fn rule(
    name: &str,
    vars: u32,
    atoms: Vec<Atom<Math>>,
    head: Vec<HeadOp<Math>>,
) -> Rule<Math> {
    Rule {
        name: name.to_string(),
        plan: Plan::compile(Query::tree(vars, 0, atoms)),
        head,
        head_vars: 0,
        post_saturation: false,
    }
}

pub(crate) fn node(template: Math, args: &[u32], class: u32) -> Atom<Math> {
    Atom::Node {
        template,
        args: args.iter().copied().collect(),
        class,
        row: None,
    }
}

pub(crate) fn insert(template: Math, args: &[u32], into: u32) -> HeadOp<Math> {
    HeadOp::Insert {
        label: LabelFill::plain(template),
        args: args.iter().copied().collect(),
        into,
    }
}

pub(crate) fn apply_all(g: &mut Engine<Math>, rules: &[Rule<Math>]) {
    for rule in rules {
        let roots = rule.plan.roots(g);
        for m in rule.plan.search(g, roots, &|_, _| true, false, &NoExterns) {
            g.apply_head(&rule.head, rule.head_vars, std::slice::from_ref(&m));
        }
    }
    g.rebuild();
}

pub(crate) fn comm_rule() -> Rule<Math> {
    rule(
        "add-comm",
        4,
        vec![node(
            Math::Add([Id::from_raw(1), Id::from_raw(2)]),
            &[1, 2],
            0,
        )],
        vec![
            insert(Math::Add([Id::from_raw(2), Id::from_raw(1)]), &[2, 1], 3),
            HeadOp::Union(0, 3),
        ],
    )
}

pub(crate) fn add_zero_rule() -> Rule<Math> {
    rule(
        "add-zero",
        3,
        vec![
            node(Math::Add([Id::from_raw(1), Id::from_raw(2)]), &[1, 2], 0),
            Atom::Literal {
                value: Math::Num(0),
                class: 2,
            },
        ],
        vec![HeadOp::Union(0, 1)],
    )
}
