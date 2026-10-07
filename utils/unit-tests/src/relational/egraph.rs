use proptest::prelude::*;
use tir_relational::{ClassId as Id, Engine, Label as ENode};

use super::test_lang::*;

#[test]
fn hash_consing_shares_identical_expressions() {
    // The key spans the children, so only the same operator over the same
    // children shares a node.
    let a = Id::from_raw(1);
    let b = Id::from_raw(2);
    let c = Id::from_raw(3);
    assert_eq!(Math::Add([a, b]).hash_cons(), Math::Add([a, b]).hash_cons());
    assert_ne!(Math::Add([a, b]).hash_cons(), Math::Add([a, c]).hash_cons());

    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let e1 = add(&mut g, a, b);
    let e2 = add(&mut g, a, b);
    assert_eq!(g.root(e1), g.root(e2));
    assert_eq!(g.nodes(e1).count(), 1);
    assert_eq!(g.total_size(), 3);
    assert_eq!(g.num_classes(), 3);
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
    g.union(a, b).unwrap();
    assert!(g.connected(a, b));
    assert!(!g.connected(a, c));
    assert_eq!(g.num_classes(), 2);

    // The same union under a scope is a hypothesis: the pop takes it back.
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = num(&mut g, 7);
    g.push_context();
    g.union(a, b).unwrap();
    assert!(g.connected(a, b));
    g.pop_context();
    assert!(!g.connected(a, b));
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

    assert_ne!(g.root(fa), g.root(fb));
    g.union(a, b).unwrap();
    g.rebuild();
    assert_eq!(g.root(fa), g.root(fb));
    assert_ne!(g.root(fb), g.root(fc));

    g.union(a, c).unwrap();
    g.rebuild();
    assert_eq!(g.root(fc), g.root(fb));
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
    g.union(fa, a).unwrap();
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
    assert_eq!(g.root(n1), g.root(n1b));
    assert_ne!(g.root(n1), g.root(n2));
    assert_eq!(g.num_classes(), 2);
}

#[test]
fn unique_nodes_never_share_or_merge() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let e1 = g.add(Math::Effect(0, [a])).class;
    let e2 = g.add(Math::Effect(0, [a])).class;
    assert_ne!(g.root(e1), g.root(e2));
    assert_eq!(g.num_classes(), 3);

    // Effects over operands that later merge still do not congruence-merge,
    // but their operand ids resolve through `find`.
    let b = sym(&mut g, 1);
    let ua = g.add(Math::Effect(1, [a])).class;
    let ub = g.add(Math::Effect(1, [b])).class;
    g.union(a, b).unwrap();
    g.rebuild();
    assert_ne!(g.root(ua), g.root(ub));
    let child = g.nodes(ua).next().unwrap().children()[0];
    assert!(g.connected(child, a));
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
    g.union(a, b).unwrap();
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
    g.union(a, b).unwrap();
    g.rebuild();

    g.push_context();
    assert!(g.connected(a, b));
    g.union(b, c).unwrap();
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
    g.union(fa, a).unwrap();
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
    g.union(a, b).unwrap();
    g.push_context();
    g.union(b, c).unwrap();
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
    g.union(a, b).unwrap();
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
    g.union(a, c).unwrap();
    g.rebuild();
    g.pop_context();

    // Back in the outer scope: re-adding the node must hit the same class, so
    // the outer scope's hash-cons survived the nested pop.
    let again = add(&mut g, a, b);
    assert_eq!(g.root(again), g.root(outer));
    assert_eq!(g.nodes(g.root(outer)).count(), 1);
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

/// A scope is a hypothesis: the classes, the nodes and the partition it
/// changed are back where they started once it is popped.
#[test]
fn pop_restores_the_base_graph() {
    // Nodes minted under the scope leave no class behind.
    {
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

    {
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
        assert_eq!(g.root(e1), g.root(e2));
        assert_eq!(g.num_classes(), base + 1);
    }

    {
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
}

/// The counts and the partition a pop restores are the enclosing scope's, not
/// always the base's.
#[test]
fn pop_restores_the_enclosing_counts_and_partition() {
    // Each pop reverts one layer of adds.
    {
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

    // An inner pop restores the outer scope's partition, not the base's.
    {
        let mut g = Engine::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let c = sym(&mut g, 2);
        let d = sym(&mut g, 3);
        g.rebuild();

        g.push_context();
        g.union(a, b).unwrap();
        g.rebuild();
        let outer = g.root(a);

        g.push_context();
        g.union(c, d).unwrap();
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

    {
        let mut g = Engine::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        g.rebuild();

        g.push_context();
        g.union(a, b).unwrap();
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
}

#[test]
fn scope_merge_aggregates_nodes_in_base_order() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    sym(&mut g, 1);
    let c = sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.union(c, a).unwrap();
    g.rebuild();
    let root = g.root(a);
    assert_eq!(g.scope_members(root), &[a, c][..]);
    let nodes: Vec<&Math> = g.nodes(root).collect();
    assert!(matches!(nodes[0], Math::Sym(0)));
    assert!(matches!(nodes[1], Math::Sym(2)));
    assert_eq!(g.num_classes(), 2);
    assert_eq!(g.total_size(), 3);

    g.pop_context();
    assert_eq!(g.num_classes(), 3);
    assert!(g.scope_members(root).is_empty());
    assert_eq!(g.nodes(g.root(a)).count(), 1);
}

#[test]
fn classes_iterate_scope_roots_at_first_member_position() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    g.rebuild();

    g.push_context();
    g.union(a, c).unwrap();
    g.rebuild();
    let seen: Vec<Id> = g.classes().map(|class| class.id()).collect();
    assert_eq!(seen, vec![g.root(a), b]);
    g.pop_context();
}

/// The dirty set names every class a scope changed, and no more.
#[test]
fn scope_dirty_names_the_classes_the_scope_changed() {
    // Without a scope nothing is a hypothesis, so nothing is dirty.
    {
        let mut g = Engine::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        g.union(a, b).unwrap();
        g.rebuild();
        assert!(g.scope_dirty().is_empty());
    }

    // The class a scoped union merged.
    {
        let mut g = Engine::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        sym(&mut g, 2);
        g.rebuild();

        g.push_context();
        g.union(a, b).unwrap();
        g.rebuild();
        assert_eq!(g.scope_dirty(), vec![g.root(a)]);
        g.pop_context();
    }

    // A class minted under the scope.
    {
        let mut g = Engine::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        g.rebuild();

        g.push_context();
        let sum = add(&mut g, a, b);
        assert_eq!(g.scope_dirty(), vec![g.root(sum)]);
        g.pop_context();
        assert!(g.scope_dirty().is_empty());
    }

    // Each pop drops its own scope's classes.
    {
        let mut g = Engine::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let c = sym(&mut g, 2);
        let d = sym(&mut g, 3);
        g.rebuild();

        g.push_context();
        g.union(a, b).unwrap();
        g.rebuild();
        g.push_context();
        g.union(c, d).unwrap();
        g.rebuild();
        assert_eq!(g.scope_dirty(), vec![g.root(a), g.root(c)]);

        g.pop_context();
        assert_eq!(g.scope_dirty(), vec![g.root(a)]);
        g.pop_context();
        assert!(g.scope_dirty().is_empty());
    }

    // `innermost_dirty` narrows that to the innermost scope alone.
    {
        let mut g = Engine::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let c = sym(&mut g, 2);
        let d = sym(&mut g, 3);
        g.rebuild();

        g.push_context();
        g.union(a, b).unwrap();
        g.rebuild();
        g.push_context();
        let sum = add(&mut g, c, d);
        g.union(c, d).unwrap();
        g.rebuild();
        let mut expected = vec![g.root(c), g.root(sum)];
        expected.sort();
        assert_eq!(g.innermost_dirty(), expected);

        g.pop_context();
        assert_eq!(g.innermost_dirty(), vec![g.root(a)]);
        g.pop_context();
        assert!(g.innermost_dirty().is_empty());
    }

    {
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
        g.union(a, b).unwrap();
        g.rebuild();
        let mut expected = vec![g.root(a), g.root(sum), g.root(outer)];
        expected.sort();
        assert_eq!(g.scope_dirty(), expected);
        g.pop_context();
    }
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
    let mut ones: Vec<Id> = g.classes_with_const(&Math::Num(1));
    ones.sort();
    assert_eq!(ones, vec![g.root(a), g.root(b)]);
    g.pop_context();
    assert!(g.classes_with_const(&Math::Num(1)).is_empty());
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
    g.union(a, b).unwrap();
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
    g.union(a, b).unwrap();
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
    let mut expected = vec![g.root(a), g.root(sum)];
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
    let survivor = g.union(a, b).unwrap();
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

    g.union(a, b).unwrap();
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
    assert_eq!(g.take_changed(), Some(vec![g.root(a)]));
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
    g.union(a, b).unwrap();
    g.rebuild();
    // The scope's own rounds still see what the scope changed.
    let inside = g.take_changed().expect("scope changes are nameable");
    assert!(inside.contains(&g.root(a)));
    g.pop_context();

    // The base is structurally back where it was, so its pending change is too —
    // and the scope's merges, which no longer hold, are gone.
    assert_eq!(g.take_changed(), Some(vec![c]));
    assert!(!g.connected(a, b));
    assert!(g.nodes(g.root(na)).count() == 1);
}

#[test]
fn nested_scopes_restore_one_layer_at_a_time() {
    let mut g = Engine::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    g.rebuild();
    g.take_changed();

    g.push_context();
    let outer = g.union(a, b).unwrap();
    g.push_context();
    let inner = sym(&mut g, 2);
    g.take_changed();
    g.pop_context();
    // Popping the inner scope restores the outer scope's log, not the base's.
    assert_eq!(g.take_changed(), Some(vec![g.root(outer)]));
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

    let changed = vec![g.root(x)];
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
    let survivor = g.union(a, b).unwrap();
    let changed = vec![survivor];
    // Both members' parents are reachable from the merged group.
    assert_eq!(g.delta(&changed, 1), sorted(&g, [survivor, na, nb]));
    g.pop_context();
}

fn sorted(g: &Engine<Math>, ids: impl IntoIterator<Item = Id>) -> Vec<Id> {
    let mut ids: Vec<Id> = ids.into_iter().map(|id| g.root(id)).collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The operator of a node, without its operands.
fn op_name(node: &Math) -> String {
    match node {
        Math::Num(n) => format!("num{n}"),
        Math::FNum(v) => format!("fnum{v:?}"),
        Math::Sym(s) => format!("sym{s}"),
        Math::Neg(_) => "neg".to_string(),
        Math::Add(_) => "add".to_string(),
        Math::Effect(kind, _) => format!("effect{kind}"),
    }
}

/// Everything a caller can observe, for the scope round-trip test.
fn state(g: &Engine<Math>) -> Vec<(u32, Vec<String>, Vec<Vec<u32>>)> {
    g.class_ids()
        .map(|class| {
            (
                class.0,
                g.nodes(class).map(op_name).collect(),
                g.rows(class)
                    .map(|row| g.children(row).iter().map(|c| g.find(*c).class.0).collect())
                    .collect(),
            )
        })
        .collect()
}

/// A random program: entry `i` applies an operator to ids built by earlier entries.
fn programs() -> impl Strategy<Value = Vec<(usize, Vec<usize>)>> {
    prop::collection::vec((0usize..4, prop::collection::vec(0usize..12, 0..2)), 1..24)
}

fn build(g: &mut Engine<Math>, program: &[(usize, Vec<usize>)]) -> Vec<Id> {
    let mut ids: Vec<Id> = Vec::new();
    for (op, args) in program {
        let arg = |slot: usize, ids: &[Id]| ids[args.get(slot).copied().unwrap_or(0) % ids.len()];
        let made = match op {
            _ if ids.is_empty() => Math::Num(0),
            0 => Math::Num(args.len() as i64),
            1 => Math::Neg([arg(0, &ids)]),
            2 => Math::Add([arg(0, &ids), arg(1, &ids)]),
            _ => Math::Effect(0, [arg(0, &ids)]),
        };
        ids.push(g.add(made).class);
    }
    ids
}

proptest! {
    #[test]
    fn rebuild_restores_the_functional_dependency(
        program in programs(),
        merges in prop::collection::vec((0usize..24, 0usize..24), 0..8),
    ) {
        let mut g: Engine<Math> = Engine::new();
        let ids = build(&mut g, &program);
        g.rebuild();
        for (a, b) in merges {
            g.union(ids[a % ids.len()], ids[b % ids.len()]).unwrap();
        }
        g.rebuild();
        // No two live rows share a label and canonical children in
        // different classes.
        let mut seen: std::collections::HashMap<(u32, Vec<u32>), u32> = Default::default();
        for class in g.class_ids() {
            for row in g.rows(class) {
                if g.node(row).is_unique() {
                    continue;
                }
                let key = (
                    g.label(row).0,
                    g.children(row).iter().map(|c| g.find(*c).class.0).collect(),
                );
                prop_assert_eq!(*seen.entry(key).or_insert(class.0), class.0);
            }
        }
    }

    #[test]
    fn the_same_program_builds_the_same_ids(program in programs()) {
        let mut one: Engine<Math> = Engine::new();
        let mut two: Engine<Math> = Engine::new();
        let a = build(&mut one, &program);
        let b = build(&mut two, &program);
        one.rebuild();
        two.rebuild();
        prop_assert_eq!(a, b);
        prop_assert_eq!(state(&one), state(&two));
    }

    #[test]
    fn a_scope_round_trip_restores_every_column(
        program in programs(),
        merges in prop::collection::vec((0usize..24, 0usize..24), 0..8),
    ) {
        let mut g: Engine<Math> = Engine::new();
        let ids = build(&mut g, &program);
        g.rebuild();
        let before = state(&g);
        g.push_context();
        for (a, b) in merges {
            g.union(ids[a % ids.len()], ids[b % ids.len()]).unwrap();
        }
        g.add(Math::Neg([ids[0]]));
        g.rebuild();
        g.pop_context();
        prop_assert_eq!(state(&g), before);
    }
}
