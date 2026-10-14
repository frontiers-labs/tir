//! The rules register allocation derives its relations by: liveness over
//! the CFG (Brandner et al. 2011 on blocks), one range per block and value
//! bounded by two lattices, then what the spiller and the PBQP graph read off
//! the ranges.

use smallvec::{SmallVec, smallvec};
use tir_relational::query::{Atom, Guard, Nested, Query, Rows, Select, Term};
use tir_relational::query::{Lattice, Out, Plan};
use tir_relational::rule::Head;
use tir_relational::{Expr, LatticeId, Scalar, Var};

use super::db::*;

// An insert of a rule's head: the table it writes, and the scalars of the
// match its key holds in the plain columns before the head's arguments.
pub(super) struct Insert {
    pub(super) table: u32,
    pub(super) plain: SmallVec<[Scalar; 1]>,
}

pub(super) struct Rule {
    pub(super) name: &'static str,
    pub(super) plan: Plan,
    pub(super) head: Vec<Head>,
    pub(super) head_vars: u32,
    // Per write of the head, its insert.
    pub(super) inserts: Vec<Option<Insert>>,
}

pub(super) struct Rules {
    pub(super) liveness: Vec<Rule>,
    // What the splits before constrained ops read (paper § 4).
    pub(super) spill: Vec<Rule>,
    pub(super) graph: Vec<Rule>,
}

fn rows(table: u32, vars: &[Var], tag: Option<Scalar>) -> Atom {
    let mut terms: SmallVec<[Term; 6]> = smallvec![Term::Any];
    if matches!(table, PREFER | FORBID | AFFINITY) {
        terms.push(Term::Any);
    }
    terms.extend(vars.iter().map(|&var| Term::Var(var)));
    Atom::Rows(Rows {
        select: Select {
            column: 0,
            param: table,
        },
        terms,
        tag,
        cell: None,
        swap: None,
        tag_within: None,
    })
}

fn lattice(column: LatticeId, key: Var, out: Scalar) -> Atom {
    Atom::Lattice(Lattice {
        column,
        key,
        out: Out::Word(out),
    })
}

fn not(table: u32, vars: &[Var]) -> Nested {
    Nested {
        atoms: vec![rows(table, vars, None)],
        guards: Vec::new(),
    }
}

fn scalar(slot: Scalar) -> Expr {
    Expr::Scalar(slot)
}

fn plus(slot: Scalar, by: i64) -> Expr {
    Expr::Add(Box::new(scalar(slot)), Box::new(Expr::Lit(by)))
}

struct Spec {
    name: &'static str,
    vars: u32,
    scalars: u32,
    atoms: Vec<Atom>,
    guards: Vec<Guard>,
    nots: Vec<Nested>,
    // The table the head inserts into, its plain scalars, its arguments and
    // whether they are an unordered pair.
    insert: (u32, &'static [Scalar], &'static [Var], bool),
    raise: Vec<(LatticeId, Expr)>,
}

fn rule(spec: Spec) -> Rule {
    let into = spec.vars;
    let (table, plain, args, sorted) = spec.insert;
    let mut head = vec![Head::Insert {
        args: args.iter().copied().collect(),
        sorted,
        into,
    }];
    let mut inserts = vec![Some(Insert {
        table,
        plain: plain.iter().copied().collect(),
    })];
    for (column, value) in spec.raise {
        head.push(Head::Raise {
            column,
            key: into,
            value,
        });
        inserts.push(None);
    }
    Rule {
        name: spec.name,
        plan: Plan::compile(Query {
            space: SPACE,
            vars: spec.vars,
            scalars: spec.scalars,
            params: TABLES,
            root: 0,
            atoms: spec.atoms,
            guards: spec.guards,
            nots: spec.nots,
        }),
        head,
        head_vars: 1,
        inserts,
    }
}

impl Rules {
    pub(super) fn new() -> Self {
        Self {
            liveness: liveness(),
            spill: vec![live_at()],
            graph: interfere()
                .into_iter()
                .chain([forbid(), affinity(PHI), affinity(COPY), live_at()])
                .collect(),
        }
    }
}

// Live-in and live-out sets, then one range per block and value: from its
// definition, or the block's entry, to its last use, or the block's exit.
fn liveness() -> Vec<Rule> {
    // A block's parameters are defined in it, so they are never live into it:
    // its predecessors pass them as the arguments their branches read.
    // A value has one defining block, so "not defined in b" is "defined in
    // a block other than b": a positive atom, which a round can search from
    // its new rows alone.
    let livein_use = Spec {
        name: "livein-use",
        vars: 6,
        scalars: 0,
        // use(op) -> v, holds(op) -> b, defined_in(d, v), d != b
        atoms: vec![
            rows(USES, &[1, 2, 0], None),
            rows(HOLDS, &[1, 3], None),
            rows(DEFINED_IN, &[4, 2, 5], None),
        ],
        guards: vec![Guard::Distinct(smallvec![(3, 4)])],
        nots: Vec::new(),
        insert: (LIVEIN, &[], &[3, 2], false),
        raise: Vec::new(),
    };
    let liveout = Spec {
        name: "liveout",
        vars: 5,
        scalars: 0,
        // livein(s) -> v, edge(p, s)
        atoms: vec![rows(LIVEIN, &[1, 2, 0], None), rows(EDGE, &[3, 1, 4], None)],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (LIVEOUT, &[], &[3, 2], false),
        raise: Vec::new(),
    };
    let livein_out = Spec {
        name: "livein-out",
        vars: 5,
        scalars: 0,
        // liveout(b) -> v, defined_in(d, v), d != b
        atoms: vec![
            rows(LIVEOUT, &[1, 2, 0], None),
            rows(DEFINED_IN, &[3, 2, 4], None),
        ],
        guards: vec![Guard::Distinct(smallvec![(1, 3)])],
        nots: Vec::new(),
        insert: (LIVEIN, &[], &[1, 2], false),
        raise: Vec::new(),
    };
    let range_def = Spec {
        name: "range-def",
        vars: 4,
        scalars: 1,
        // def(op) -> v, holds(op) -> b at point p
        atoms: vec![rows(DEFS, &[1, 2, 0], None), rows(HOLDS, &[1, 3], Some(0))],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (RANGE, &[], &[3, 2], false),
        raise: vec![(START, plus(0, 1)), (END, plus(0, 2))],
    };
    let range_use = Spec {
        name: "range-use",
        vars: 4,
        scalars: 1,
        atoms: vec![rows(USES, &[1, 2, 0], None), rows(HOLDS, &[1, 3], Some(0))],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (RANGE, &[], &[3, 2], false),
        raise: vec![(END, scalar(0))],
    };
    let range_param = Spec {
        name: "range-param",
        vars: 3,
        scalars: 0,
        atoms: vec![rows(PARAM, &[1, 2, 0], None)],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (RANGE, &[], &[1, 2], false),
        raise: vec![(START, Expr::Lit(1)), (END, Expr::Lit(2))],
    };
    let range_in = Spec {
        name: "range-in",
        vars: 3,
        scalars: 0,
        atoms: vec![rows(LIVEIN, &[1, 2, 0], None)],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (RANGE, &[], &[1, 2], false),
        raise: vec![(START, Expr::Lit(0))],
    };
    let range_out = Spec {
        name: "range-out",
        vars: 3,
        scalars: 0,
        atoms: vec![rows(LIVEOUT, &[1, 2, 0], None)],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (RANGE, &[], &[1, 2], false),
        raise: vec![(END, Expr::Lit(EXIT))],
    };
    [
        livein_use,
        liveout,
        livein_out,
        range_def,
        range_use,
        range_param,
        range_in,
        range_out,
    ]
    .into_iter()
    .map(rule)
    .collect()
}

// The ops of block `b` whose point `p` is in `[lo, hi)`, read off the
// block's ops by their points rather than tried one by one.
fn ops_within(b: Var, op: Var, p: Scalar, lo: Expr, hi: Expr) -> Atom {
    let Atom::Rows(held) = rows(HOLDS, &[op, b], Some(p)) else {
        unreachable!("a row atom")
    };
    Atom::Rows(Rows {
        tag_within: Some((lo, hi)),
        ..held
    })
}

// The values in a register after each op: defined at or before it, read
// after it.
fn live_at() -> Rule {
    rule(Spec {
        name: "live-at",
        vars: 4,
        scalars: 3,
        // range(b, v) -> r, holds(op) -> b at point p, start(r) <= p + 1 < p + 2 < end(r)
        atoms: vec![
            rows(RANGE, &[1, 2, 0], None),
            lattice(START, 0, 1),
            lattice(END, 0, 2),
            ops_within(1, 3, 0, plus(1, -1), plus(2, -2)),
        ],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (LIVE_AT, &[], &[3, 2], false),
        raise: Vec::new(),
    })
}

// Two ranges of one block that overlap: one starts inside the other. The
// ends of a copy whose destination nothing else defines hold one value, and
// do not interfere through it.
//
// A range starts at a definition of its value or on the block's entry, so
// two overlap where one holds its value at a point the other's is defined
// at, or where both are open on entry: live in, or a parameter.
fn interfere() -> Vec<Rule> {
    let defined = rule(Spec {
        name: "interfere",
        vars: 8,
        scalars: 3,
        // range(b, a) -> r, holds(op) -> b at point p, start(r) <= p + 1 < end(r),
        // def(op) -> c
        atoms: vec![
            rows(RANGE, &[1, 2, 0], None),
            lattice(START, 0, 1),
            lattice(END, 0, 2),
            ops_within(1, 3, 0, plus(1, -1), plus(2, -1)),
            rows(DEFS, &[3, 4, 5], None),
        ],
        guards: vec![Guard::Distinct(smallvec![(2, 4)])],
        nots: vec![not(SAME, &[2, 4, 6]), not(SAME, &[4, 2, 7])],
        insert: (INTERFERE, &[], &[2, 4], true),
        raise: Vec::new(),
    });
    let entry = |name, first, second| {
        rule(Spec {
            name,
            vars: 7,
            scalars: 0,
            // first(b) -> a, second(b) -> c
            atoms: vec![
                rows(first, &[1, 2, 0], None),
                rows(second, &[1, 3, 4], None),
            ],
            guards: vec![Guard::Distinct(smallvec![(2, 3)])],
            nots: vec![not(SAME, &[2, 3, 5]), not(SAME, &[3, 2, 6])],
            insert: (INTERFERE, &[], &[2, 3], true),
            raise: Vec::new(),
        })
    };
    vec![
        defined,
        entry("interfere-in", LIVEIN, LIVEIN),
        entry("interfere-in-param", LIVEIN, PARAM),
        entry("interfere-param", PARAM, PARAM),
    ]
}

// A value in a register after an op that clobbers one avoids it.
fn forbid() -> Rule {
    rule(Spec {
        name: "forbid",
        vars: 4,
        scalars: 4,
        // range(b, v) -> r, holds(op) -> b at point p, start(r) < p + 2 < end(r),
        // clobber(op) tag reg
        atoms: vec![
            rows(RANGE, &[1, 2, 0], None),
            lattice(START, 0, 2),
            lattice(END, 0, 3),
            ops_within(1, 3, 1, plus(2, -1), plus(3, -2)),
            rows(CLOBBER, &[3, 3], Some(0)),
        ],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (FORBID, &[0], &[2], false),
        raise: Vec::new(),
    })
}

// A phi row joins an argument to its parameter, and a copy row its source
// to its destination, at the weight of the edge or block that would run the
// copy.
fn affinity(table: u32) -> Rule {
    let (name, atom) = match table {
        // phi(pred, b) -> a tag w
        PHI => ("affinity-phi", rows(PHI, &[1, 2, 0], Some(0))),
        // copy(a, b) -> c tag w
        _ => ("affinity-copy", rows(COPY, &[1, 2, 0], Some(0))),
    };
    let args: &'static [Var] = match table {
        PHI => &[0, 2],
        _ => &[1, 2],
    };
    rule(Spec {
        name,
        vars: 3,
        scalars: 1,
        atoms: vec![atom],
        guards: Vec::new(),
        nots: Vec::new(),
        insert: (AFFINITY, &[0], args, false),
        raise: Vec::new(),
    })
}
