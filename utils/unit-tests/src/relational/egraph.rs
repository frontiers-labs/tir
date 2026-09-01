use tir_relational::{ClassId as Id, Engine, Label as ENode};

use super::test_lang::*;

#[test]
fn hash_consing_shares_identical_expressions() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let e1 = add(&mut g, a, b);
    let e2 = add(&mut g, a, b);
    assert_eq!(g.find(e1), g.find(e2));
    assert_eq!(g.nodes(e1).count(), 1);
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
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    assert!(g.lookup(&Math::Add([a, b])).is_none());
    assert_eq!(g.num_classes(), 2);
    let e = add(&mut g, a, b);
    assert_eq!(g.lookup(&Math::Add([a, b])), Some(g.find(e)));
}

#[test]
fn union_merges_classes() {
    let mut g = Engine::new();
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
    let mut g = Engine::new();
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
    let mut g = Engine::new();
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
    let mut g = Engine::new();
    let n1 = num(&mut g, 1);
    let n2 = num(&mut g, 2);
    let n1b = num(&mut g, 1);
    assert_eq!(g.find(n1), g.find(n1b));
    assert_ne!(g.find(n1), g.find(n2));
    assert_eq!(g.num_classes(), 2);
}

#[test]
fn unique_nodes_never_share_or_merge() {
    let mut g = Engine::new();
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
    let child = g.nodes(ua).next().unwrap().children()[0];
    assert!(g.connected(child, a));
}

#[test]
fn scope_union_is_discarded_on_pop() {
    let mut g = Engine::new();
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
    let mut g = Engine::new();
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
    let mut g = Engine::new();
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
    let mut g = Engine::new();
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
    let mut g = Engine::new();
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
    // A node built inside a scope participates in scoped congruence. `b` is
    // interned first so it represents the merged set, which is what leaves
    // `neg(b)` a term the base hash-cons has never seen.
    let mut g = Engine::new();
    let b = sym(&mut g, 1);
    let a = sym(&mut g, 0);
    let fa = neg(&mut g, a);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    let fb = neg(&mut g, b);
    assert_ne!(fa, fb);
    g.rebuild();
    assert!(g.connected(fa, fb));
    g.pop_context();
    // fb's base singleton lingers but is no longer equal to fa.
    assert!(!g.connected(fa, fb));
}

#[test]
fn nested_pop_restores_outer_scope_hash_cons() {
    let mut g = Engine::new();
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
    assert_eq!(g.nodes(g.find(outer)).count(), 1);
}

#[test]
fn rewrite_under_scope_is_discarded_on_pop() {
    // add(x, y) => add(y, x), applied only inside a scope.
    let comm = comm_rule();

    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    g.rebuild();
    assert!(!g.connected(ab, ba));

    g.push_context();
    apply_all(&mut g, &[comm]);
    assert!(g.connected(ab, ba));
    g.pop_context();
    assert!(!g.connected(ab, ba));
}

#[test]
fn scope_add_then_pop_restores_class_count() {
    let mut g = Engine::new();
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
    let mut g = Engine::new();
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
    let mut g = Engine::new();
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
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    add(&mut g, a, b);
    g.rebuild();
    let base_classes = g.num_classes();
    let base_size = g.total_size();

    g.push_context();
    g.saturate_rules(&[comm], &tir_relational::NoExterns, 10, 1000);
    assert!(g.total_size() > base_size);
    g.pop_context();

    assert_eq!(g.num_classes(), base_classes);
    assert_eq!(g.total_size(), base_size);
}

#[test]
fn scope_merge_aggregates_nodes_in_base_order() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    sym(&mut g, 1);
    let c = sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.union(c, a);
    g.rebuild();
    let root = g.find(a);
    assert_eq!(g.scope_members(root), &[a, c][..]);
    let nodes: Vec<&Math> = g.nodes(root).collect();
    assert!(matches!(nodes[0], Math::Sym(0)));
    assert!(matches!(nodes[1], Math::Sym(2)));
    assert_eq!(g.num_classes(), 2);
    assert_eq!(g.total_size(), 3);

    g.pop_context();
    assert_eq!(g.num_classes(), 3);
    assert!(g.scope_members(root).is_empty());
    assert_eq!(g.nodes(g.find(a)).count(), 1);
}

#[test]
fn nested_pop_restores_outer_scope_partition() {
    let mut g = Engine::new();
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
    assert_eq!(g.nodes(outer).count(), 2);
    assert!(!g.connected(c, d));

    g.pop_context();
    assert_eq!(g.num_classes(), 4);
}

#[test]
fn scope_counts_follow_the_hypothesis_and_the_pop_undoes_them() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    // A merge counts the moment it happens; congruence repair is what waits for
    // the rebuild.
    assert_eq!(g.num_classes(), 1);
    assert_eq!(g.total_size(), 2);
    g.rebuild();
    assert_eq!(g.num_classes(), 1);
    assert_eq!(g.total_size(), 2);
    g.pop_context();
    assert_eq!(g.num_classes(), 2);
    assert_eq!(g.total_size(), 2);
}

#[test]
fn classes_iterate_scope_roots_at_first_member_position() {
    let mut g = Engine::new();
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

#[test]
fn scope_dirty_is_empty_without_a_scope() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.union(a, b);
    g.rebuild();
    assert!(g.scope_dirty().is_empty());
}

#[test]
fn scope_dirty_holds_the_class_a_scoped_union_merged() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    g.rebuild();
    assert_eq!(g.scope_dirty(), vec![g.find(a)]);
    g.pop_context();
}

#[test]
fn scope_dirty_holds_a_class_minted_under_the_scope() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();

    g.push_context();
    let sum = add(&mut g, a, b);
    assert_eq!(g.scope_dirty(), vec![g.find(sum)]);
    g.pop_context();
    assert!(g.scope_dirty().is_empty());
}

#[test]
fn scope_dirty_drops_the_inner_scope_on_pop() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    let d = sym(&mut g, 3);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    g.rebuild();
    g.push_context();
    g.union(c, d);
    g.rebuild();
    assert_eq!(g.scope_dirty(), vec![g.find(a), g.find(c)]);

    g.pop_context();
    assert_eq!(g.scope_dirty(), vec![g.find(a)]);
    g.pop_context();
    assert!(g.scope_dirty().is_empty());
}

#[test]
fn innermost_dirty_holds_only_the_inner_scope_changes() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    let d = sym(&mut g, 3);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    g.rebuild();
    g.push_context();
    let sum = add(&mut g, c, d);
    g.union(c, d);
    g.rebuild();
    let mut expected = vec![g.find(c), g.find(sum)];
    expected.sort();
    assert_eq!(g.innermost_dirty(), expected);

    g.pop_context();
    assert_eq!(g.innermost_dirty(), vec![g.find(a)]);
    g.pop_context();
    assert!(g.innermost_dirty().is_empty());
}

#[test]
fn scope_dirty_closes_upward_over_parents() {
    // A parent's e-nodes re-canonicalize through the merge, so a pattern rooted
    // there can match under the scope and not in the base graph.
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let sum = add(&mut g, a, b);
    let outer = neg(&mut g, sum);
    sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.union(a, b);
    g.rebuild();
    let mut expected = vec![g.find(a), g.find(sum), g.find(outer)];
    expected.sort();
    assert_eq!(g.scope_dirty(), expected);
    g.pop_context();
}

#[test]
fn assume_const_is_read_under_the_scope_and_gone_after_pop() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    g.rebuild();

    assert!(g.const_of(a).is_none());
    g.push_context();
    g.assume_const(a, Math::Num(1));
    assert!(matches!(g.const_of(a), Some(Math::Num(1))));
    g.pop_context();
    assert!(g.const_of(a).is_none());
}

#[test]
fn assumed_classes_names_every_class_assumed_to_be_the_constant() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.assume_const(a, Math::Num(1));
    g.assume_const(b, Math::Num(1));
    g.assume_const(c, Math::Num(0));
    let mut ones: Vec<Id> = g.classes_with_const(&Math::Num(1)).collect();
    ones.sort();
    assert_eq!(ones, vec![g.find(a), g.find(b)]);
    g.pop_context();
    assert!(g.classes_with_const(&Math::Num(1)).next().is_none());
}

/// A nested scope that assumes the opposite of its parent has assumed a
/// contradiction, and the column says so: facts join, they do not shadow. A
/// conflicted class reads as nothing known, which is the conservative answer a
/// block proven unreachable needs.
#[test]
fn a_nested_assumption_conflicts_with_the_outer_one_and_the_pop_restores_it() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    g.rebuild();

    g.push_context();
    g.assume_const(a, Math::Num(1));
    g.push_context();
    g.assume_const(a, Math::Num(0));
    assert!(g.const_of(a).is_none());
    assert!(g.const_conflicted(a));
    g.pop_context();
    assert!(matches!(g.const_of(a), Some(Math::Num(1))));
    g.pop_context();
    assert!(g.const_of(a).is_none());
}

#[test]
fn assumption_follows_the_class_through_a_scoped_union() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();

    g.push_context();
    g.assume_const(a, Math::Num(1));
    g.union(a, b);
    g.rebuild();
    assert!(matches!(g.const_of(b), Some(Math::Num(1))));
    g.pop_context();
    assert!(g.const_of(a).is_none());
    assert!(g.const_of(b).is_none());
}

#[test]
fn inner_union_rekeys_an_outer_assumption_and_pop_restores_it() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();

    g.push_context();
    g.assume_const(a, Math::Num(1));
    g.push_context();
    g.union(a, b);
    g.rebuild();
    assert!(matches!(g.const_of(b), Some(Math::Num(1))));
    g.pop_context();
    assert!(matches!(g.const_of(a), Some(Math::Num(1))));
    assert!(g.const_of(b).is_none());
    g.pop_context();
}

#[test]
fn scope_dirty_holds_an_assumed_class_and_its_parents() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let sum = add(&mut g, a, b);
    sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.assume_const(a, Math::Num(0));
    let mut expected = vec![g.find(a), g.find(sum)];
    expected.sort();
    assert_eq!(g.scope_dirty(), expected);
    g.pop_context();
    assert!(g.scope_dirty().is_empty());
}

#[test]
#[should_panic(expected = "a scope to be undone by")]
fn assume_const_without_a_scope_panics() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    g.assume_const(a, Math::Num(1));
}

// ── Change log ─────────────────────────────────────────────────────────────

#[test]
fn take_changed_starts_as_everything() {
    let mut g: Engine<Math> = Engine::new();
    assert!(g.take_changed().is_none());
    assert_eq!(g.take_changed(), Some(Vec::new()));
}

#[test]
fn added_classes_are_changed() {
    let mut g = Engine::new();
    g.take_changed();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let sum = add(&mut g, a, b);
    assert_eq!(g.take_changed(), Some(sorted(&g, [a, b, sum])));
    assert_eq!(g.take_changed(), Some(Vec::new()));
}

#[test]
fn union_survivor_is_changed() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.take_changed();
    let survivor = g.union(a, b);
    assert_eq!(g.take_changed(), Some(vec![survivor]));
}

#[test]
fn repair_reports_re_canonicalized_parents() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let na = neg(&mut g, a);
    let nb = neg(&mut g, b);
    g.rebuild();
    g.take_changed();

    g.union(a, b);
    g.rebuild();
    // The merge itself, and the parents congruence then merged: one of them was
    // re-canonicalized onto the other.
    assert_eq!(g.take_changed(), Some(sorted(&g, [a, na, nb])));
}

#[test]
fn assumed_constants_change_their_class() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    g.take_changed();
    g.push_context();
    g.assume_const(a, Math::Num(1));
    assert_eq!(g.take_changed(), Some(vec![g.find(a)]));
    g.pop_context();
    // The assumption went with the scope, so nothing is left to re-search.
    assert_eq!(g.take_changed(), Some(Vec::new()));
}

#[test]
fn a_scope_leaves_the_change_log_as_it_found_it() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let na = neg(&mut g, a);
    g.rebuild();
    g.take_changed();

    // One base change the enclosing driver has not drained yet.
    let c = sym(&mut g, 2);

    g.push_context();
    g.assume_const(a, Math::Num(1));
    g.union(a, b);
    g.rebuild();
    // The scope's own rounds still see what the scope changed.
    let inside = g.take_changed().expect("scope changes are nameable");
    assert!(inside.contains(&g.find(a)));
    g.pop_context();

    // The base is structurally back where it was, so its pending change is too —
    // and the scope's merges, which no longer hold, are gone.
    assert_eq!(g.take_changed(), Some(vec![c]));
    assert!(!g.connected(a, b));
    assert!(g.nodes(g.find(na)).count() == 1);
}

#[test]
fn nested_scopes_restore_one_layer_at_a_time() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();
    g.take_changed();

    g.push_context();
    let outer = g.union(a, b);
    g.push_context();
    let inner = sym(&mut g, 2);
    g.take_changed();
    g.pop_context();
    // Popping the inner scope restores the outer scope's log, not the base's.
    assert_eq!(g.take_changed(), Some(vec![g.find(outer)]));
    let _ = inner;
    g.pop_context();
    assert_eq!(g.take_changed(), Some(Vec::new()));
}

#[test]
fn delta_closes_upward_by_height() {
    let mut g = Engine::new();
    let x = sym(&mut g, 0);
    let hx = neg(&mut g, x);
    let ghx = neg(&mut g, hx);
    let fghx = neg(&mut g, ghx);
    g.rebuild();

    let changed = vec![g.find(x)];
    assert_eq!(g.delta(&changed, 0), sorted(&g, [x]));
    assert_eq!(g.delta(&changed, 1), sorted(&g, [x, hx]));
    assert_eq!(g.delta(&changed, 2), sorted(&g, [x, hx, ghx]));
    assert_eq!(g.delta(&changed, 3), sorted(&g, [x, hx, ghx, fghx]));
}

#[test]
fn delta_covers_a_merged_group_parents_under_a_scope() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let na = neg(&mut g, a);
    let nb = neg(&mut g, b);
    g.rebuild();

    g.push_context();
    let survivor = g.union(a, b);
    let changed = vec![survivor];
    // Both members' parents are reachable from the merged group.
    assert_eq!(g.delta(&changed, 1), sorted(&g, [survivor, na, nb]));
    g.pop_context();
}

fn sorted(g: &Engine<Math>, ids: impl IntoIterator<Item = Id>) -> Vec<Id> {
    let mut ids: Vec<Id> = ids.into_iter().map(|id| g.find(id)).collect();
    ids.sort();
    ids.dedup();
    ids
}
