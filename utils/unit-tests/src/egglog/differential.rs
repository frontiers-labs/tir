//! Random programs against independent references: datalog saturated by
//! tir-egglog against a naive fixpoint over hash sets, and a commutative
//! theory saturated in relaxed mode against strict mode.

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;
use tir_egglog::prelude::*;
use tir_egglog::{Database, Mode, Rules, Schedule, SortKind, TableId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Term {
    Var(u8),
    Const(i64),
}

type Atom = (usize, Vec<Term>);

#[derive(Clone, Debug)]
struct Datalog {
    arities: Vec<usize>,
    rules: Vec<(Vec<Atom>, Atom)>,
    facts: Vec<(usize, Vec<i64>)>,
}

fn term() -> impl Strategy<Value = Term> {
    prop_oneof![3 => (0u8..4).prop_map(Term::Var), 1 => (0i64..3).prop_map(Term::Const)]
}

fn atom(arities: Vec<usize>) -> impl Strategy<Value = Atom> {
    (0..arities.len())
        .prop_flat_map(move |r| (Just(r), proptest::collection::vec(term(), arities[r])))
}

fn datalog() -> impl Strategy<Value = Datalog> {
    proptest::collection::vec(1usize..=3, 3..=4).prop_flat_map(|arities| {
        let body = proptest::collection::vec(atom(arities.clone()), 1..=4);
        let rule = (body, atom(arities.clone())).prop_map(|(body, (head, args))| {
            // A head variable the body does not bind becomes a constant.
            let bound = |v: &u8| body.iter().any(|(_, args)| args.contains(&Term::Var(*v)));
            let args = (args.into_iter())
                .map(|t| match t {
                    Term::Var(v) if !bound(&v) => Term::Const(0),
                    t => t,
                })
                .collect();
            (body, (head, args))
        });
        let fact = (0..arities.len()).prop_flat_map({
            let arities = arities.clone();
            move |r| (Just(r), proptest::collection::vec(0i64..3, arities[r]))
        });
        let rules = proptest::collection::vec(rule, 1..=4);
        let facts = proptest::collection::vec(fact, 0..=12);
        (Just(arities), rules, facts).prop_map(|(arities, rules, facts)| Datalog {
            arities,
            rules,
            facts,
        })
    })
}

// Every assignment of the body's variables that the facts satisfy.
fn matches(
    body: &[Atom],
    facts: &[HashSet<Vec<i64>>],
    bound: &HashMap<u8, i64>,
    out: &mut Vec<HashMap<u8, i64>>,
) {
    let Some(((relation, args), rest)) = body.split_first() else {
        out.push(bound.clone());
        return;
    };
    for fact in &facts[*relation] {
        let mut next = bound.clone();
        let fits = args.iter().zip(fact).all(|(term, &value)| match *term {
            Term::Const(c) => c == value,
            Term::Var(v) => *next.entry(v).or_insert(value) == value,
        });
        if fits {
            matches(rest, facts, &next, out);
        }
    }
}

fn naive(program: &Datalog) -> Vec<HashSet<Vec<i64>>> {
    let mut facts = vec![HashSet::new(); program.arities.len()];
    for (relation, row) in &program.facts {
        facts[*relation].insert(row.clone());
    }
    loop {
        let mut new = Vec::new();
        for (body, (head, args)) in &program.rules {
            let mut found = Vec::new();
            matches(body, &facts, &HashMap::new(), &mut found);
            for assignment in found {
                let row: Vec<i64> = (args.iter())
                    .map(|t| match *t {
                        Term::Const(c) => c,
                        Term::Var(v) => assignment[&v],
                    })
                    .collect();
                if !facts[*head].contains(&row) {
                    new.push((*head, row));
                }
            }
        }
        if new.is_empty() {
            return facts;
        }
        for (relation, row) in new {
            facts[relation].insert(row);
        }
    }
}

fn expr(name: &str, args: &[Term]) -> Expr {
    let arg = |t: &Term| match *t {
        Term::Var(v) => exprs::var(&format!("v{v}")),
        Term::Const(c) => exprs::int(c),
    };
    exprs::call(name, args.iter().map(arg).collect())
}

fn egglog(program: &Datalog) -> Vec<HashSet<Vec<i64>>> {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let relations: Vec<TableId> = (program.arities.iter().enumerate())
        .map(|(r, &arity)| db.add_relation(&format!("r{r}"), &vec![int; arity]))
        .collect();
    let name = |r: usize| format!("r{r}");
    let mut rules = Rules::new();
    for (body, (head, args)) in &program.rules {
        let facts = Facts(
            body.iter()
                .map(|(r, args)| Fact::Fact(expr(&name(*r), args)))
                .collect(),
        );
        let actions = GenericActions(vec![Action::Expr(span!(), expr(&name(*head), args))]);
        rules.rule(&db, "", facts, actions).unwrap();
    }
    for (relation, row) in &program.facts {
        let row: Vec<_> = row.iter().map(|&v| db.base(v)).collect();
        db.insert(relations[*relation], &row).unwrap();
    }
    let mut compiled = rules.compile(&db).unwrap();
    compiled.run(&mut db, &Schedule::saturate("")).unwrap();
    (relations.iter())
        .map(|&table| {
            let rows = db.rows(table);
            rows.map(|row| row.values().map(|v| db.base_value::<i64>(v)).collect())
                .collect()
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn datalog_saturates_to_the_naive_fixpoint(program in datalog()) {
        prop_assert_eq!(egglog(&program), naive(&program));
    }

    #[test]
    fn relaxed_and_strict_commutativity_reach_the_same_fixpoint(
        terms in proptest::collection::vec(math_term(), 1..=4),
    ) {
        let sizes = |mode| -> Vec<usize> {
            let (mut db, mut rules) = commutative();
            rules.set_mode("", mode);
            for t in &terms {
                build(&mut db, t);
            }
            let mut program = rules.compile(&db).unwrap();
            program.run(&mut db, &Schedule::saturate("")).unwrap();
            let add = db.schema().table(db.table("Add").unwrap()).symmetries().to_vec();
            let expected: &[Vec<usize>] = if mode == Mode::Relaxed { &[vec![1, 0, 2]] } else { &[] };
            assert_eq!(add, expected);
            let count = |name| db.rows(db.table(name).unwrap()).count();
            ["Num", "Add", "Mul"].into_iter().map(count).collect()
        };
        prop_assert_eq!(sizes(Mode::Relaxed), sizes(Mode::Strict));
    }
}

#[derive(Clone, Debug)]
enum Math {
    Num(i64),
    Add(Box<Math>, Box<Math>),
    Mul(Box<Math>, Box<Math>),
}

fn math_term() -> impl Strategy<Value = Math> {
    let leaf = (0i64..3).prop_map(Math::Num);
    leaf.prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Math::Add(Box::new(a), Box::new(b))),
            (inner.clone(), inner).prop_map(|(a, b)| Math::Mul(Box::new(a), Box::new(b))),
        ]
    })
}

fn build(db: &mut Database, term: &Math) -> tir_egglog::Value {
    let (name, args) = match term {
        Math::Num(n) => ("Num", vec![db.base(*n)]),
        Math::Add(a, b) => ("Add", vec![build(db, a), build(db, b)]),
        Math::Mul(a, b) => ("Mul", vec![build(db, a), build(db, b)]),
    };
    let table = db.table(name).unwrap();
    db.entry(table, &args).unwrap()
}

// Commutative addition, distribution of multiplication over it, and the
// factoring back, whose two premise atoms the symmetry of Add makes
// interchangeable.
fn commutative() -> (Database, Rules) {
    let mut db = Database::new();
    let math = db.add_sort("Math", SortKind::Eq);
    let int = db.sort("i64").unwrap();
    db.add_constructor("Num", &[int], math);
    db.add_constructor("Add", &[math, math], math);
    db.add_constructor("Mul", &[math, math], math);
    let mut rules = Rules::new();
    let mut rw = |lhs: Expr, rhs: Expr| rules.rewrite(&db, "", lhs, rhs).unwrap();
    rw(expr!((Add a b)), expr!((Add b a)));
    rw(expr!((Mul a (Add b c))), expr!((Add (Mul a b) (Mul a c))));
    rw(expr!((Add (Mul a b) (Mul a c))), expr!((Mul a (Add b c))));
    (db, rules)
}
