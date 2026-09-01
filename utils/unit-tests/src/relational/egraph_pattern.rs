//! What a query reads off the e-graph that the engine's own tests cannot see:
//! the [`ENode`] contracts a language has to keep, and the legality hook a
//! caller prunes with.

use tir_adt::APInt;
use tir_relational::{Atom, ClassId, NoExterns, Plan, Query};
use tir_relational::{ClassId as Id, Engine, Label as ENode};

use super::test_lang::*;

/// `matches` looser than `hash_cons`: a `WILD` tag matches any tag, so the
/// operator index must key on [`ENode::op_key`] (tag dropped), not `hash_cons`.
#[derive(Clone, Debug)]
enum Wild {
    Leaf(u32),
    Op(u32, [Id; 1]),
}

impl Wild {
    const WILD: u32 = u32::MAX;
}

impl ENode for Wild {
    fn children(&self) -> &[Id] {
        match self {
            Wild::Leaf(_) => &[],
            Wild::Op(_, c) => c,
        }
    }
    fn children_mut(&mut self) -> &mut [Id] {
        match self {
            Wild::Leaf(_) => &mut [],
            Wild::Op(_, c) => c,
        }
    }
    fn hash_cons(&self) -> u64 {
        let mut hash = match self {
            Wild::Leaf(s) => *s as u64,
            Wild::Op(tag, _) => 1 << 32 | *tag as u64,
        };
        for child in self.children() {
            hash = hash.rotate_left(5) ^ child.index() as u64;
        }
        hash
    }
    fn op_key(&self) -> u64 {
        match self {
            Wild::Leaf(s) => *s as u64,
            Wild::Op(..) => 1 << 32,
        }
    }
    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Wild::Leaf(a), Wild::Leaf(b)) => a == b,
            (Wild::Op(a, _), Wild::Op(b, _)) => a == b || *a == Self::WILD || *b == Self::WILD,
            _ => false,
        }
    }
}

/// A template that matches any tag must still find the classes holding the
/// operator, which is what [`ENode::op_key`]'s contract buys.
#[test]
fn the_operator_index_finds_a_wildcard_rooted_match() {
    let mut g: Engine<Wild> = Engine::new();
    let leaf = g.add(Wild::Leaf(7));
    let op = g.add(Wild::Op(5, [leaf]));

    let plan = Plan::compile(Query::tree(
        2,
        0,
        vec![Atom::Node {
            template: Wild::Op(Wild::WILD, [ClassId::from_raw(1)]),
            args: smallvec::smallvec![1],
            class: 0,
            row: None,
        }],
    ));
    let roots = plan.roots(&g);
    let found = plan.search(&g, roots, &|_, _| true, false, &NoExterns);
    assert_eq!(found.len(), 1);
    assert_eq!(g.find(found[0].root), g.find(op));
    assert_eq!(found[0].bindings[1], Some(g.find(leaf)));
}

/// The legality hook prunes a binding the caller rejects, wherever it sits.
#[test]
fn the_legality_hook_prunes_a_binding() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    g.rebuild();

    let plan = Plan::compile(Query::tree(
        3,
        0,
        vec![node(
            Math::Add([Id::from_raw(1), Id::from_raw(2)]),
            &[1, 2],
            0,
        )],
    ));
    let roots = plan.roots(&g);
    assert_eq!(
        plan.search(&g, roots.clone(), &|_, _| true, false, &NoExterns)
            .len(),
        1
    );
    let rejected = g.find(b);
    assert!(plan
        .search(&g, roots, &|_, class| class != rejected, false, &NoExterns)
        .is_empty());
    let _ = ab;
}

/// A literal operand reads what the class is *known* to be, which under a scope
/// is what the scope assumed of it rather than a row it holds.
#[test]
fn a_literal_matches_the_constant_a_scope_assumed() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let root = add(&mut g, a, b);
    g.rebuild();

    let plan = Plan::compile(Query::tree(
        3,
        0,
        vec![
            node(Math::Add([Id::from_raw(1), Id::from_raw(2)]), &[1, 2], 0),
            Atom::Literal {
                value: Math::Num(0),
                class: 2,
            },
        ],
    ));
    let roots: Vec<Id> = vec![root];
    assert!(plan
        .search(&g, roots.clone(), &|_, _| true, false, &NoExterns)
        .is_empty());

    g.push_context();
    g.assume_const(b, Math::Num(0));
    let found = plan.search(&g, roots.clone(), &|_, _| true, false, &NoExterns);
    assert_eq!(found.len(), 1);
    g.pop_context();
    assert!(plan
        .search(&g, roots, &|_, _| true, false, &NoExterns)
        .is_empty());
}

/// A float literal reads the same column an integer one does.
#[test]
fn a_float_literal_matches_its_constant() {
    let mut g = Engine::new();
    let f = fnum(&mut g, 1.5);
    let n = neg(&mut g, f);
    g.rebuild();

    let plan = Plan::compile(Query::tree(
        2,
        0,
        vec![
            node(Math::Neg([Id::from_raw(1)]), &[1], 0),
            Atom::Literal {
                value: Math::FNum(tir_adt::APFloat::from_f64(1.5)),
                class: 1,
            },
        ],
    ));
    let roots = plan.roots(&g);
    assert_eq!(
        plan.search(&g, roots, &|_, _| true, false, &NoExterns)
            .len(),
        1
    );
    let _ = (n, APInt::from_i64(0));
}
