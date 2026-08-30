use tir_relational::{Atom, HeadOp, NoExterns, Rule};
use tir_symbolic::egraph::{EGraph, ENode, Id};

use super::test_lang::*;

// ── Rules ──────────────────────────────────────────────────────────────────

#[test]
fn double_negation_eliminates() {
    // neg(neg(x)) => x
    let double_neg = rule(
        "double-neg",
        3,
        vec![
            node(Math::Neg([Id::from_raw(1)]), &[1], 0),
            node(Math::Neg([Id::from_raw(2)]), &[2], 1),
        ],
        vec![HeadOp::Union(0, 2)],
    );

    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let nn = neg(&mut g, a);
    let nna = neg(&mut g, nn);
    assert!(!g.connected(nna, a));

    apply_all(&mut g, &[double_neg]);
    assert!(g.connected(nna, a));
}

#[test]
fn commutativity_unions_swapped_form() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    assert!(!g.connected(ab, ba));

    apply_all(&mut g, &[comm_rule()]);
    assert!(g.connected(ab, ba));
}

#[test]
fn additive_identity_via_integer_literal() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let z = num(&mut g, 0);
    let root = add(&mut g, a, z);
    assert!(!g.connected(root, a));

    apply_all(&mut g, &[add_zero_rule()]);
    assert!(g.connected(root, a));
}

// ── Saturation ─────────────────────────────────────────────────────────────

#[test]
fn saturates_and_applies_a_rule() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    assert!(!g.connected(ab, ba));

    g.saturate_rules(&[comm_rule()], &NoExterns, 30, 100_000);
    assert!(g.connected(ab, ba));
}

#[test]
fn combines_rules_across_iterations() {
    // add(0, a): commutativity exposes add(a, 0), then add-zero collapses it.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let z = num(&mut g, 0);
    let root = add(&mut g, z, a);
    assert!(!g.connected(root, a));

    g.saturate_rules(&[comm_rule(), add_zero_rule()], &NoExterns, 30, 100_000);
    assert!(g.connected(root, a));
}

#[test]
fn iter_limit_zero_does_nothing() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    let classes = g.num_classes();

    g.saturate_rules(&[comm_rule()], &NoExterns, 0, 100_000);
    assert!(!g.connected(ab, ba));
    assert_eq!(g.num_classes(), classes);
}

#[test]
fn node_limit_halts_before_growth() {
    // comm must mint add(b, a); capping at the current size blocks it.
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    add(&mut g, a, b);
    let size = g.total_size();

    g.saturate_rules(&[comm_rule()], &NoExterns, 30, size);
    assert_eq!(g.total_size(), size);
}

#[test]
fn roots_canonicalize_after_saturation() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);

    g.saturate_rules(&[comm_rule()], &NoExterns, 30, 100_000);
    assert_eq!(g.find(ab), g.find(ba));
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
    assert_eq!(chosen.children(), g.nodes(root).next().unwrap().children());
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

// ── Semi-naive saturation ──────────────────────────────────────────────────

/// The rules the property test draws subsets from: heights 1 and 2, growing and
/// shrinking, so a round's delta is neither always empty nor always everything.
fn math_rules() -> Vec<Rule<Math>> {
    vec![
        comm_rule(),
        add_zero_rule(),
        // neg(neg(x)) => x
        rule(
            "neg-neg",
            3,
            vec![
                node(Math::Neg([Id::from_raw(1)]), &[1], 0),
                node(Math::Neg([Id::from_raw(2)]), &[2], 1),
            ],
            vec![HeadOp::Union(0, 2)],
        ),
        // neg(add(x, y)) => add(neg(x), neg(y))
        rule(
            "neg-add",
            7,
            vec![
                node(Math::Neg([Id::from_raw(1)]), &[1], 0),
                node(Math::Add([Id::from_raw(2), Id::from_raw(3)]), &[2, 3], 1),
            ],
            vec![
                insert(Math::Neg([Id::from_raw(2)]), &[2], 4),
                insert(Math::Neg([Id::from_raw(3)]), &[3], 5),
                insert(Math::Add([Id::from_raw(4), Id::from_raw(5)]), &[4, 5], 6),
                HeadOp::Union(0, 6),
            ],
        ),
        // add(add(x, y), z) => add(x, add(y, z))
        rule(
            "add-assoc",
            7,
            vec![
                node(Math::Add([Id::from_raw(1), Id::from_raw(2)]), &[1, 2], 0),
                node(Math::Add([Id::from_raw(3), Id::from_raw(4)]), &[3, 4], 1),
            ],
            vec![
                insert(Math::Add([Id::from_raw(4), Id::from_raw(2)]), &[4, 2], 5),
                insert(Math::Add([Id::from_raw(3), Id::from_raw(5)]), &[3, 5], 6),
                HeadOp::Union(0, 6),
            ],
        ),
        // add(x, x) => neg(neg(add(x, x)))
        rule(
            "double-wrap",
            4,
            vec![node(
                Math::Add([Id::from_raw(1), Id::from_raw(1)]),
                &[1, 1],
                0,
            )],
            vec![
                insert(Math::Neg([Id::from_raw(0)]), &[0], 2),
                insert(Math::Neg([Id::from_raw(2)]), &[2], 3),
                HeadOp::Union(0, 3),
            ],
        ),
    ]
}

/// The pre-semi-naive driver: every round searches every rule over the whole
/// graph. The reference the delta rounds must agree with.
fn saturate_naive(
    g: &mut EGraph<Math>,
    rules: &[&Rule<Math>],
    iter_limit: usize,
    node_limit: usize,
) {
    for _ in 0..iter_limit {
        let size = g.total_size();
        if size >= node_limit {
            break;
        }
        let before = (g.num_classes(), size, g.stats().raises);
        let searched: Vec<_> = rules
            .iter()
            .map(|rule| {
                let roots = rule.plan.roots(g);
                (
                    *rule,
                    rule.plan.search(g, roots, &|_, _| true, false, &NoExterns),
                )
            })
            .collect();
        for (rule, matches) in &searched {
            for m in matches {
                g.apply_head(&rule.head, rule.head_vars, m);
            }
        }
        g.rebuild();
        if (g.num_classes(), g.total_size(), g.stats().raises) == before {
            break;
        }
    }
}

/// A random expression over two symbols and the constants 0 and 1.
#[derive(Debug, Clone)]
enum Expr {
    Leaf(u32),
    Zero,
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
}

fn build(g: &mut EGraph<Math>, e: &Expr) -> Id {
    match e {
        Expr::Leaf(s) => sym(g, *s),
        Expr::Zero => num(g, 0),
        Expr::Neg(a) => {
            let a = build(g, a);
            neg(g, a)
        }
        Expr::Add(a, b) => {
            let a = build(g, a);
            let b = build(g, b);
            add(g, a, b)
        }
    }
}

/// Cost of the expression extraction chose at `id` — the number it minimized.
/// Not the shape: two saturations reaching the same partition hold different sets
/// of equivalent spellings, so they break a cost tie differently.
fn extracted_cost(
    g: &EGraph<Math>,
    e: &tir_symbolic::egraph::Extraction<'_, Math>,
    id: Id,
    seen: &mut Vec<Id>,
) -> Option<u64> {
    let id = g.find(id);
    if seen.contains(&id) {
        return None;
    }
    let node = e.node(id)?;
    seen.push(id);
    let mut total = unit(node);
    for &child in node.children() {
        total += extracted_cost(g, e, child, seen)?;
    }
    seen.pop();
    Some(total)
}

fn expr_strategy() -> impl proptest::strategy::Strategy<Value = Expr> {
    use proptest::prelude::*;
    let leaf = prop_oneof![(0u32..2).prop_map(Expr::Leaf), Just(Expr::Zero)];
    leaf.prop_recursive(4, 24, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(|a| Expr::Neg(Box::new(a))),
            (inner.clone(), inner).prop_map(|(a, b)| Expr::Add(Box::new(a), Box::new(b))),
        ]
    })
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(500))]

    /// Semi-naive rounds skip roots whose downward cone did not change; the
    /// fixpoint they reach must be the one a full search per round reaches.
    #[test]
    fn semi_naive_equals_naive(
        exprs in proptest::collection::vec(expr_strategy(), 1..4),
        mask in 1u32..64,
    ) {
        let all = math_rules();
        let rules: Vec<&Rule<Math>> = all
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, rule)| rule)
            .collect();

        let seed = |g: &mut EGraph<Math>| -> Vec<Id> {
            exprs.iter().map(|e| build(g, e)).collect()
        };

        let mut naive = EGraph::new();
        let naive_roots = seed(&mut naive);
        naive.rebuild();
        saturate_naive(&mut naive, &rules, 30, 10_000);

        let mut semi = EGraph::new();
        let semi_roots = seed(&mut semi);
        semi.rebuild();
        let owned: Vec<Rule<Math>> = rules.iter().map(|&rule| rule.clone()).collect();
        semi.saturate_rules(&owned, &NoExterns, 30, 10_000);

        // Not the class count: a driver that stops on the iteration limit
        // rather than at a fixpoint stops wherever its own round schedule put
        // it, and semi-naive reaches a growing graph's limit a round or so off
        // the full search. What must agree is what the graph *proves* — the
        // partition over the seeded roots and the cost of extracting them.
        for (i, &a) in naive_roots.iter().enumerate() {
            for (j, &b) in naive_roots.iter().enumerate() {
                proptest::prop_assert_eq!(
                    naive.connected(a, b),
                    semi.connected(semi_roots[i], semi_roots[j])
                );
            }
        }
        let naive_extraction = naive.extract_best(|_, node| unit(node));
        let semi_extraction = semi.extract_best(|_, node| unit(node));
        for (i, &root) in naive_roots.iter().enumerate() {
            proptest::prop_assert_eq!(
                extracted_cost(&naive, &naive_extraction, root, &mut Vec::new()),
                extracted_cost(&semi, &semi_extraction, semi_roots[i], &mut Vec::new())
            );
        }
    }
}

/// A rule reaching an atom sideways cannot have its roots narrowed to the change
/// frontier: the row it reaches sits in a sibling class of the root, so nothing
/// the log closes upward names the root when that row is minted.
#[test]
fn a_sideways_rule_still_fires_on_a_row_a_later_round_minted() {
    use smallvec::smallvec;
    use tir_relational::{LabelFill, Plan, Query};

    let mut g: EGraph<Math> = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let sum = add(&mut g, a, b);
    g.add(Math::Effect(0, [a]));
    g.rebuild();

    // `effect(x)` proves `neg(x)` worth having; it is minted in round 0's apply
    // phase, so round 1 is the first that can see it.
    let mint = Rule {
        name: "mint-neg".into(),
        plan: Plan::compile(Query::tree(
            3,
            0,
            vec![Atom::Node {
                template: Math::Effect(0, [Id::from_raw(1)]),
                args: smallvec![1],
                class: 0,
                row: None,
            }],
        )),
        head: vec![HeadOp::Insert {
            label: LabelFill::plain(Math::Neg([Id::from_raw(0)])),
            args: smallvec![1],
            into: 2,
        }],
        head_vars: 0,
        post_saturation: false,
    };
    // `add(x, y)` with a `neg(x)` anywhere in the graph proves the two equal —
    // nonsense as algebra, and exactly the shape of a law relating two spellings.
    let relate = Rule {
        name: "relate".into(),
        plan: Plan::compile(Query::tree(
            4,
            0,
            vec![
                Atom::Node {
                    template: Math::Add([Id::from_raw(1), Id::from_raw(2)]),
                    args: smallvec![1, 2],
                    class: 0,
                    row: None,
                },
                Atom::Node {
                    template: Math::Neg([Id::from_raw(1)]),
                    args: smallvec![1],
                    class: 3,
                    row: None,
                },
            ],
        )),
        head: vec![HeadOp::Union(0, 3)],
        head_vars: 0,
        post_saturation: false,
    };
    assert!(relate.plan.unbounded());

    g.saturate_rules(&[mint, relate], &tir_relational::NoExterns, 10, 1000);

    let negated = g.add(Math::Neg([a]));
    assert!(g.connected(sum, negated));
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(200))]

    /// A scope extracts by recomputing the classes its assumption dirtied and
    /// reading the rest off the base extraction. The answer must be the one a
    /// full pass over the scoped graph gives, node for node.
    #[test]
    fn refresh_equals_full_extraction(
        exprs in proptest::collection::vec(expr_strategy(), 1..4),
        pairs in proptest::collection::vec((0usize..64, 0usize..64), 0..4),
    ) {
        let rules = math_rules();
        let mut g = EGraph::new();
        for e in &exprs {
            build(&mut g, e);
        }
        g.rebuild();
        g.saturate_rules(&rules, &NoExterns, 30, 10_000);
        let base = g.extract_best(|_, node| unit(node));

        let classes: Vec<Id> = g.class_ids().collect();
        g.push_context();
        for &(a, b) in &pairs {
            g.union(classes[a % classes.len()], classes[b % classes.len()]);
        }
        g.rebuild();
        g.saturate_rules(&rules, &NoExterns, 30, 10_000);

        let refreshed = base.refresh(&g, &g.scope_dirty(), |_, node| unit(node));
        let full = g.extract_best(|_, node| unit(node));
        for id in g.class_ids() {
            proptest::prop_assert_eq!(
                refreshed.node(id).map(|node| format!("{node:?}")),
                full.node(id).map(|node| format!("{node:?}"))
            );
        }
        g.pop_context();
    }
}
