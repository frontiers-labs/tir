use tir_symbolic::egraph::{EGraph, ENode, Id, Pattern, Rewrite, Rhs, Runner, Substitution, Var};

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
fn math_rules() -> Vec<Rewrite<Math, &'static str>> {
    fn pattern(build: impl Fn(&mut Pattern<Math, &'static str>)) -> Pattern<Math, &'static str> {
        let mut p = Pattern::new();
        build(&mut p);
        p
    }
    let rule = |name, lhs, rhs| Rewrite::new(name, lhs, Rhs::Pattern(rhs));
    vec![
        comm_rule(),
        add_zero_rule(),
        // neg(neg(x)) => x
        rule(
            "neg-neg",
            pattern(|p| {
                let x = p.var(Var::Symbol("x"));
                let inner = p.add(Math::Neg([x]));
                p.add(Math::Neg([inner]));
            }),
            pattern(|p| {
                p.var(Var::Symbol("x"));
            }),
        ),
        // neg(add(x, y)) => add(neg(x), neg(y))
        rule(
            "neg-add",
            pattern(|p| {
                let x = p.var(Var::Symbol("x"));
                let y = p.var(Var::Symbol("y"));
                let sum = p.add(Math::Add([x, y]));
                p.add(Math::Neg([sum]));
            }),
            pattern(|p| {
                let x = p.var(Var::Symbol("x"));
                let y = p.var(Var::Symbol("y"));
                let nx = p.add(Math::Neg([x]));
                let ny = p.add(Math::Neg([y]));
                p.add(Math::Add([nx, ny]));
            }),
        ),
        // add(add(x, y), z) => add(x, add(y, z))
        rule(
            "add-assoc",
            pattern(|p| {
                let x = p.var(Var::Symbol("x"));
                let y = p.var(Var::Symbol("y"));
                let z = p.var(Var::Symbol("z"));
                let inner = p.add(Math::Add([x, y]));
                p.add(Math::Add([inner, z]));
            }),
            pattern(|p| {
                let x = p.var(Var::Symbol("x"));
                let y = p.var(Var::Symbol("y"));
                let z = p.var(Var::Symbol("z"));
                let inner = p.add(Math::Add([y, z]));
                p.add(Math::Add([x, inner]));
            }),
        ),
        // add(x, x) => neg(neg(add(x, x)))
        rule(
            "double-wrap",
            pattern(|p| {
                let x = p.var(Var::Symbol("x"));
                p.add(Math::Add([x, x]));
            }),
            pattern(|p| {
                let x = p.var(Var::Symbol("x"));
                let sum = p.add(Math::Add([x, x]));
                let inner = p.add(Math::Neg([sum]));
                p.add(Math::Neg([inner]));
            }),
        ),
    ]
}

/// Today's driver: every round searches every rule over the whole graph. The
/// reference semi-naive saturation must agree with.
fn saturate_naive(
    g: &mut EGraph<Math>,
    rules: &[&Rewrite<Math, &'static str>],
    iter_limit: usize,
    node_limit: usize,
) {
    for _ in 0..iter_limit {
        let size = g.total_size();
        if size >= node_limit {
            break;
        }
        let before = (g.num_classes(), size);
        let searched: Vec<_> = rules
            .iter()
            .map(|rule| (*rule, rule.lhs.search(g)))
            .collect();
        for (rule, matches) in &searched {
            for m in matches {
                rule.apply_match(g, m);
            }
        }
        g.rebuild();
        if (g.num_classes(), g.total_size()) == before {
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
    e: &tir_symbolic::egraph::Extraction<Math>,
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
        let rules: Vec<&Rewrite<Math, &'static str>> = all
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
        semi.saturate(rules.iter().copied(), 30, 10_000);

        proptest::prop_assert_eq!(naive.num_classes(), semi.num_classes());
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

/// A rule whose left-hand side binds one level but whose applier reads three:
/// the shape of instcombine's memory laws, where the pattern is a bare
/// `Load(..)`/`Store(..)` and the applier walks the address and state chains
/// below it. The applier declines until `deep` holds the marker constant.
fn deep_read_rule() -> Rewrite<Math, &'static str> {
    let mut lhs: Pattern<Math, &'static str> = Pattern::new();
    let x = lhs.var(Var::Symbol("x"));
    lhs.add(Math::Neg([x]));

    let apply = move |g: &mut EGraph<Math>, subst: &Substitution<&'static str>, root: Id| {
        let x = subst.get(&Var::Symbol("x")).expect("bound x");
        // root -> x -> left -> deep: two levels past what the pattern binds.
        let Some(left) = child_of_add(g, g.find(x), 0) else {
            return;
        };
        let Some(deep) = child_of_add(g, left, 0) else {
            return;
        };
        if !g.nodes(deep).any(|n| matches!(n, Math::Num(7))) {
            return;
        }
        let marker = g.add(Math::Num(99));
        g.union(root, marker);
    };
    Rewrite::new("deep-read", lhs, Rhs::Apply(Box::new(apply)))
}

fn child_of_add(g: &EGraph<Math>, class: Id, slot: usize) -> Option<Id> {
    g.nodes(class).find_map(|node| match node {
        Math::Add(kids) => Some(g.find(kids[slot])),
        _ => None,
    })
}

/// `sym(1) => 7`, which is what lets `deep-read` stop declining — one round late.
fn sym_becomes_seven() -> Rewrite<Math, &'static str> {
    let mut lhs: Pattern<Math, &'static str> = Pattern::new();
    lhs.add(Math::Sym(1));
    let mut rhs: Pattern<Math, &'static str> = Pattern::new();
    rhs.add(Math::Num(7));
    Rewrite::new("sym1-is-7", lhs, Rhs::Pattern(rhs))
}

#[test]
fn applier_reading_past_the_pattern_is_still_re_searched() {
    // neg(add(add(sym1, sym2), sym0)): the class `deep-read` inspects sits three
    // levels under the root it rewrites.
    let seed = |g: &mut EGraph<Math>| {
        let s0 = sym(g, 0);
        let s1 = sym(g, 1);
        let s2 = sym(g, 2);
        let inner = add(g, s1, s2);
        let outer = add(g, inner, s0);
        neg(g, outer)
    };
    let rules = [deep_read_rule(), sym_becomes_seven()];
    let borrowed: Vec<&Rewrite<Math, &'static str>> = rules.iter().collect();

    let mut naive = EGraph::new();
    let naive_root = seed(&mut naive);
    naive.rebuild();
    saturate_naive(&mut naive, &borrowed, 30, 10_000);

    let mut semi = EGraph::new();
    let semi_root = seed(&mut semi);
    semi.rebuild();
    semi.saturate(borrowed.iter().copied(), 30, 10_000);

    let holds_marker =
        |g: &EGraph<Math>, root: Id| g.nodes(g.find(root)).any(|n| matches!(n, Math::Num(99)));
    assert!(holds_marker(&naive, naive_root), "naive must fire the rule");
    assert!(
        holds_marker(&semi, semi_root),
        "semi-naive skipped a rule whose applier reads past its pattern"
    );
}
