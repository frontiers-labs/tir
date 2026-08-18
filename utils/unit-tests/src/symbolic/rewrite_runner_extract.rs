use tir_symbolic::egraph::{EGraph, ENode, Pattern, Rewrite, Rhs, Runner, Var};

use super::test_lang::*;

// ── Rewrites ───────────────────────────────────────────────────────────────

#[test]
fn double_negation_eliminates_via_declarative_rhs() {
    // neg(neg(x)) => x
    let mut lhs: Pattern<Math, &'static str> = Pattern::new();
    let x = lhs.var(Var::Symbol("x"));
    let inner = lhs.add(Math::Neg([x]));
    lhs.add(Math::Neg([inner]));

    let mut rhs: Pattern<Math, &'static str> = Pattern::new();
    rhs.var(Var::Symbol("x"));

    let rule = Rewrite::new("double-neg", lhs, Rhs::Pattern(rhs));

    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let nn = neg(&mut g, a);
    let nna = neg(&mut g, nn);
    assert!(!g.connected(nna, a));

    rule.apply_all(&mut g);
    assert!(g.connected(nna, a));
}

#[test]
fn commutativity_unions_swapped_form() {
    // add(x, y) => add(y, x)
    let rule = comm_rule();

    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    assert!(!g.connected(ab, ba));

    rule.apply_all(&mut g);
    assert!(g.connected(ab, ba));
}

#[test]
fn additive_identity_via_integer_literal() {
    // add(x, 0) => x
    let rule = add_zero_rule();

    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let z = num(&mut g, 0);
    let root = add(&mut g, a, z);
    assert!(!g.connected(root, a));

    rule.apply_all(&mut g);
    assert!(g.connected(root, a));
}

#[test]
fn imperative_applier_unions_root_with_binding() {
    // neg(x) => x via a closure (degenerate; just exercises the escape hatch).
    let mut lhs: Pattern<Math, &'static str> = Pattern::new();
    let x = lhs.var(Var::Symbol("x"));
    lhs.add(Math::Neg([x]));

    let rule = Rewrite::new(
        "neg-id",
        lhs,
        Rhs::Apply(Box::new(|eg, subst, root| {
            let x = subst.get(&Var::Symbol("x")).unwrap();
            eg.union(root, x);
        })),
    );

    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let na = neg(&mut g, a);
    assert!(!g.connected(na, a));

    rule.apply_all(&mut g);
    assert!(g.connected(na, a));
}

// ── Runner ─────────────────────────────────────────────────────────────────

#[test]
fn saturates_and_applies_a_rule() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    assert!(!g.connected(ab, ba));

    let mut runner = Runner::new(g, vec![]);
    runner.run(&[comm_rule()]);
    assert!(runner.egraph().connected(ab, ba));
}

#[test]
fn combines_rules_across_iterations() {
    // add(0, a): commutativity exposes add(a, 0), then add-zero collapses it.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let z = num(&mut g, 0);
    let root = add(&mut g, z, a);
    assert!(!g.connected(root, a));

    let mut runner = Runner::new(g, vec![]);
    runner.run(&[comm_rule(), add_zero_rule()]);
    assert!(runner.egraph().connected(root, a));
}

#[test]
fn iter_limit_zero_does_nothing() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    let classes = g.num_classes();

    let mut runner = Runner::new(g, vec![]).with_iter_limit(0);
    runner.run(&[comm_rule()]);
    assert!(!runner.egraph().connected(ab, ba));
    assert_eq!(runner.egraph().num_classes(), classes);
}

#[test]
fn node_limit_halts_before_growth() {
    // comm must mint add(b, a); capping at the current size blocks it.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    add(&mut g, a, b);
    let size = g.total_size();

    let mut runner = Runner::new(g, vec![]).with_node_limit(size);
    runner.run(&[comm_rule()]);
    assert_eq!(runner.egraph().total_size(), size);
}

#[test]
fn roots_canonicalize_after_saturation() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);

    let mut runner = Runner::new(g, vec![ab, ba]);
    runner.run(&[comm_rule()]);
    let roots = runner.roots();
    assert_eq!(roots[0], roots[1]);
}

// ── Extraction ─────────────────────────────────────────────────────────────

/// Unit cost for operators, zero for leaves.
fn unit(node: &Math) -> u64 {
    match node {
        Math::Num(_) | Math::FNum(_) | Math::Sym(_) => 0,
        _ => 1,
    }
}

#[test]
fn picks_cheaper_equivalent_form() {
    // neg(neg(a)) unioned with a: extraction prefers the bare a (cost 0).
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let inner = neg(&mut g, a);
    let nn = neg(&mut g, inner);
    g.union(nn, a);
    g.rebuild();

    let extraction = g.extract_best(|_, node| unit(node));
    assert!(matches!(extraction.node(g.find(a)).unwrap(), Math::Sym(0)));
}

#[test]
fn sums_children_costs() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let na = neg(&mut g, a);
    let extraction = g.extract_best(|_, node| unit(node));
    // neg(a) costs 1 (op) + 0 (leaf) = 1; the chosen node is the neg.
    assert!(matches!(extraction.node(g.find(na)).unwrap(), Math::Neg(_)));
}

#[test]
fn ties_keep_the_first_node_to_reach_the_minimum() {
    // add(a, b) and add(b, a) merged: equal cost, so the node the scan reaches
    // first stays chosen.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    g.union(ab, ba);
    g.rebuild();

    let extraction = g.extract_best(|_, node| unit(node));
    let root = g.find(ab);
    let chosen = extraction.node(root).unwrap();
    assert_eq!(chosen.children(), g.nodes(root)[0].children());
}

#[test]
fn terminates_on_a_cycle() {
    // a ≡ neg(a): the class is a self-cycle, but extraction still terminates and
    // costs it through the symbol leaf.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let na = neg(&mut g, a);
    g.union(a, na);
    g.rebuild();
    let extraction = g.extract_best(|_, node| unit(node));
    assert!(extraction.node(g.find(a)).is_some());
}
