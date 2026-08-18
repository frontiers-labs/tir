use tir_symbolic::egraph::{EGraph, ENode, Id};

use super::test_lang::*;

#[test]
fn hash_consing_shares_identical_expressions() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let e1 = add(&mut g, a, b);
    let e2 = add(&mut g, a, b);
    assert_eq!(g.find(e1), g.find(e2));
    assert_eq!(g.nodes(e1).len(), 1);
    assert_eq!(g.total_size(), 3);
    assert_eq!(g.num_classes(), 3);
}

#[test]
fn hash_cons_includes_children() {
    let a = Id::from_raw(1);
    let b = Id::from_raw(2);
    let c = Id::from_raw(3);

    assert_eq!(Math::Add([a, b]).hash_cons(), Math::Add([a, b]).hash_cons());
    assert_ne!(Math::Add([a, b]).hash_cons(), Math::Add([a, c]).hash_cons());
}

#[test]
fn lookup_probes_without_inserting() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    assert!(g.lookup(&Math::Add([a, b])).is_none());
    assert_eq!(g.num_classes(), 2);
    let e = add(&mut g, a, b);
    assert_eq!(g.lookup(&Math::Add([a, b])), Some(g.find(e)));
}

#[test]
fn union_merges_classes() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = num(&mut g, 7);
    let c = num(&mut g, 9);
    assert_eq!(g.num_classes(), 3);
    g.union(a, b);
    assert!(g.connected(a, b));
    assert!(!g.connected(a, c));
    assert_eq!(g.num_classes(), 2);
}

#[test]
fn congruence_merges_function_applications() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    let fa = neg(&mut g, a);
    let fb = neg(&mut g, b);
    let fc = neg(&mut g, c);

    assert_ne!(g.find(fa), g.find(fb));
    g.union(a, b);
    g.rebuild();
    assert_eq!(g.find(fa), g.find(fb));
    assert_ne!(g.find(fb), g.find(fc));

    g.union(a, c);
    g.rebuild();
    assert_eq!(g.find(fc), g.find(fb));
}

#[test]
fn rebuild_propagates_congruence_to_fixpoint() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let mut cur = a;
    for _ in 0..5 {
        cur = neg(&mut g, cur);
    }
    let fa = neg(&mut g, a);
    assert_eq!(g.num_classes(), 6);
    g.union(fa, a);
    g.rebuild();
    assert_eq!(g.num_classes(), 1);
}

#[test]
fn hash_collision_keeps_distinct_nodes_separate() {
    // Num(1) and Num(2) share a hash_cons bucket but must not merge.
    let mut g = EGraph::new();
    let n1 = num(&mut g, 1);
    let n2 = num(&mut g, 2);
    let n1b = num(&mut g, 1);
    assert_eq!(g.find(n1), g.find(n1b));
    assert_ne!(g.find(n1), g.find(n2));
    assert_eq!(g.num_classes(), 2);
}

#[test]
fn unique_nodes_never_share_or_merge() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let e1 = g.add(Math::Effect(0, [a]));
    let e2 = g.add(Math::Effect(0, [a]));
    assert_ne!(g.find(e1), g.find(e2));
    assert_eq!(g.num_classes(), 3);

    // Effects over operands that later merge still do not congruence-merge,
    // but their operand ids resolve through `find`.
    let b = sym(&mut g, 1);
    let ua = g.add(Math::Effect(1, [a]));
    let ub = g.add(Math::Effect(1, [b]));
    g.union(a, b);
    g.rebuild();
    assert_ne!(g.find(ua), g.find(ub));
    let child = g.nodes(ua)[0].children()[0];
    assert!(g.connected(child, a));
}

#[test]
fn scope_union_is_discarded_on_pop() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = num(&mut g, 7);
    g.push_context();
    g.union(a, b);
    assert!(g.connected(a, b));
    g.pop_context();
    assert!(!g.connected(a, b));
}

#[test]
fn scope_congruence_collapses_and_restores() {
    // neg(a) and neg(b) are distinct at base; assuming a≡b in a scope makes
    // them congruent, and popping restores the distinction.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let fa = neg(&mut g, a);
    let fb = neg(&mut g, b);
    g.rebuild();
    assert!(!g.connected(fa, fb));

    g.push_context();
    g.union(a, b);
    g.rebuild();
    assert!(g.connected(a, b));
    assert!(g.connected(fa, fb));

    g.pop_context();
    assert!(!g.connected(a, b));
    assert!(!g.connected(fa, fb));
}

#[test]
fn scope_preserves_base_equalities() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    g.union(a, b);
    g.rebuild();

    g.push_context();
    assert!(g.connected(a, b));
    g.union(b, c);
    g.rebuild();
    assert!(g.connected(a, c));
    g.pop_context();

    assert!(g.connected(a, b));
    assert!(!g.connected(a, c));
}

#[test]
fn scope_congruence_propagates_to_fixpoint() {
    // neg(neg(a)) ≡ a under a≡neg(a): assuming a≡neg(a) collapses the whole
    // tower of negations into one class.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let mut cur = a;
    for _ in 0..5 {
        cur = neg(&mut g, cur);
    }
    let fa = neg(&mut g, a);
    g.rebuild();
    let base_classes = g.num_classes();

    g.push_context();
    g.union(fa, a);
    g.rebuild();
    assert_eq!(g.num_classes(), 1);
    g.pop_context();
    assert_eq!(g.num_classes(), base_classes);
}

#[test]
fn nested_scopes_isolate() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    g.push_context();
    g.union(a, b);
    g.push_context();
    g.union(b, c);
    g.rebuild();
    assert!(g.connected(a, c));
    g.pop_context();
    assert!(g.connected(a, b));
    assert!(!g.connected(a, c));
    g.pop_context();
    assert!(!g.connected(a, b));
}

#[test]
fn scope_add_then_congruence() {
    // A node built inside a scope participates in scoped congruence.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let fa = neg(&mut g, a);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    let fb = neg(&mut g, b);
    g.rebuild();
    assert!(g.connected(fa, fb));
    g.pop_context();
    // fb's base singleton lingers but is no longer equal to fa.
    assert!(!g.connected(fa, fb));
}

#[test]
fn nested_pop_restores_outer_scope_hash_cons() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();

    g.push_context();
    let outer = add(&mut g, a, b); // interned in the outer scope's hash-cons
    g.push_context();
    let c = sym(&mut g, 2);
    g.union(a, c);
    g.rebuild();
    g.pop_context();

    // Back in the outer scope: re-adding the node must hit the same class, so
    // the outer scope's hash-cons survived the nested pop.
    let again = add(&mut g, a, b);
    assert_eq!(g.find(again), g.find(outer));
    assert_eq!(g.nodes(g.find(outer)).len(), 1);
}

#[test]
fn rewrite_under_scope_is_discarded_on_pop() {
    // add(x, y) => add(y, x), applied only inside a scope.
    let comm = comm_rule();

    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    g.rebuild();
    assert!(!g.connected(ab, ba));

    g.push_context();
    comm.apply_all(&mut g);
    assert!(g.connected(ab, ba));
    g.pop_context();
    assert!(!g.connected(ab, ba));
}

#[test]
fn scope_add_then_pop_restores_class_count() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();
    let base = g.num_classes();

    g.push_context();
    neg(&mut g, a);
    add(&mut g, a, b);
    g.rebuild();
    assert_eq!(g.num_classes(), base + 2);
    g.pop_context();
    assert_eq!(g.num_classes(), base);
}

#[test]
fn readd_after_pop_mints_one_class_no_accumulation() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();
    let base = g.num_classes();

    g.push_context();
    add(&mut g, a, b);
    g.pop_context();

    // The scope's node is gone from the base memo, so re-adding mints exactly one
    // fresh class; it is then interned, so a repeat shares it (no accumulation).
    let e1 = add(&mut g, a, b);
    assert_eq!(g.num_classes(), base + 1);
    let e2 = add(&mut g, a, b);
    assert_eq!(g.find(e1), g.find(e2));
    assert_eq!(g.num_classes(), base + 1);
}

#[test]
fn nested_scope_pop_reverts_only_inner_adds() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();
    let base = g.num_classes();

    g.push_context();
    neg(&mut g, a);
    g.rebuild();
    assert_eq!(g.num_classes(), base + 1);
    g.push_context();
    add(&mut g, a, b);
    g.rebuild();
    assert_eq!(g.num_classes(), base + 2);
    g.pop_context();
    assert_eq!(g.num_classes(), base + 1);
    g.pop_context();
    assert_eq!(g.num_classes(), base);
}

#[test]
fn scoped_saturate_leaves_base_identical() {
    // Commutativity introduces add(b, a) as a new node inside the scope; after pop
    // the base graph must be structurally identical.
    let comm = comm_rule();
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    add(&mut g, a, b);
    g.rebuild();
    let base_classes = g.num_classes();
    let base_size = g.total_size();

    g.push_context();
    g.saturate([&comm], 10, 1000);
    assert!(g.total_size() > base_size);
    g.pop_context();

    assert_eq!(g.num_classes(), base_classes);
    assert_eq!(g.total_size(), base_size);
}

#[test]
fn scope_merge_aggregates_nodes_in_base_order() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    sym(&mut g, 1);
    let c = sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.union(c, a);
    g.rebuild();
    let root = g.find(a);
    assert_eq!(g.scope_members(root), &[a, c][..]);
    let nodes = g.nodes(root);
    assert!(matches!(nodes[0], Math::Sym(0)));
    assert!(matches!(nodes[1], Math::Sym(2)));
    assert_eq!(g.num_classes(), 2);
    assert_eq!(g.total_size(), 3);

    g.pop_context();
    assert_eq!(g.num_classes(), 3);
    assert!(g.scope_members(root).is_empty());
    assert_eq!(g.nodes(g.find(a)).len(), 1);
}

#[test]
fn nested_pop_restores_outer_scope_partition() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    let d = sym(&mut g, 3);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    g.rebuild();
    let outer = g.find(a);

    g.push_context();
    g.union(c, d);
    g.rebuild();
    assert_eq!(g.num_classes(), 2);
    g.pop_context();

    assert_eq!(g.num_classes(), 3);
    assert_eq!(g.total_size(), 4);
    assert_eq!(g.scope_members(outer), &[a, b][..]);
    assert_eq!(g.nodes(outer).len(), 2);
    assert!(!g.connected(c, d));

    g.pop_context();
    assert_eq!(g.num_classes(), 4);
}

#[test]
fn scope_class_view_is_snapshot_until_rebuild() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    // No rebuild yet: the aggregated view still shows the pre-union partition.
    assert_eq!(g.num_classes(), 2);
    assert_eq!(g.total_size(), 2);
    g.rebuild();
    assert_eq!(g.num_classes(), 1);
    assert_eq!(g.total_size(), 2);
    g.pop_context();
}

#[test]
fn classes_iterate_scope_roots_at_first_member_position() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.union(a, c);
    g.rebuild();
    let seen: Vec<Id> = g.classes().map(|class| class.id()).collect();
    assert_eq!(seen, vec![g.find(a), b]);
    g.pop_context();
}
