use tir_adt::{APFloat, APInt};
use tir_symbolic::egraph::{EGraph, ENode, Id, Pattern, PatternNode, Substitution, Var};

use super::test_lang::*;

/// `matches` looser than `hash_cons`: a `WILD` tag matches any tag, so the operator index must key on [`ENode::op_key`] (tag dropped), not `hash_cons`.
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

#[test]
fn index_finds_wildcard_rooted_match() {
    let mut g: EGraph<Wild> = EGraph::new();
    let leaf = g.add(Wild::Leaf(7));
    let op = g.add(Wild::Op(5, [leaf]));

    let mut p: Pattern<Wild, &'static str> = Pattern::new();
    let x = p.var(Var::Symbol("x"));
    p.add(Wild::Op(Wild::WILD, [x]));

    let matches = p.search(&g);
    assert_eq!(matches.len(), 1);
    assert_eq!(g.find(matches[0].root), g.find(op));
    assert_eq!(matches[0].subst.get(&Var::Symbol("x")), Some(g.find(leaf)));
}

/// `add(x, y)` with `x`, `y` symbol holes.
fn add_pattern() -> Pattern<Math, &'static str> {
    let mut p = Pattern::new();
    let x = p.var(Var::Symbol("x"));
    let y = p.var(Var::Symbol("y"));
    p.add(Math::Add([x, y]));
    p
}

#[test]
fn search_binds_operands() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let root = add(&mut g, a, b);

    let matches = add_pattern().search(&g);
    assert_eq!(matches.len(), 1);
    let m = &matches[0];
    assert_eq!(g.find(m.root), g.find(root));
    assert_eq!(m.subst.get(&Var::Symbol("x")), Some(g.find(a)));
    assert_eq!(m.subst.get(&Var::Symbol("y")), Some(g.find(b)));
}

#[test]
fn search_roots_only_visits_requested_classes() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    let requested = add(&mut g, a, b);
    add(&mut g, b, c);

    let matches = add_pattern().search_roots(&g, [requested]);
    assert_eq!(matches.len(), 1);
    assert_eq!(g.find(matches[0].root), g.find(requested));
}

#[test]
fn search_rejects_wrong_operator_and_arity() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    neg(&mut g, a);

    // Add wants two children; Neg has one and a different operator.
    assert!(add_pattern().search(&g).is_empty());
}

#[test]
fn nonlinear_pattern_requires_equal_operands() {
    // add(x, x) matches add(a, a) but not add(a, b).
    let mut p: Pattern<Math, &'static str> = Pattern::new();
    let x = p.var(Var::Symbol("x"));
    p.add(Math::Add([x, x]));

    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    add(&mut g, a, b);
    assert!(p.search(&g).is_empty());

    add(&mut g, a, a);
    let matches = p.search(&g);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].subst.get(&Var::Symbol("x")), Some(g.find(a)));
}

#[test]
fn instantiate_builds_under_substitution() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);

    let mut subst = Substitution::new();
    subst.insert(Var::Symbol("x"), a);
    subst.insert(Var::Symbol("y"), b);

    let built = add_pattern().instantiate(&mut g, &subst);
    // Hash-consing means rebuilding add(a, b) lands on the original class.
    let original = add(&mut g, a, b);
    assert_eq!(g.find(built), g.find(original));
}

/// `add(x, 0)` with a literal integer leaf.
fn add_zero_pattern() -> Pattern<Math, &'static str> {
    let mut p = Pattern::new();
    let x = p.var(Var::Symbol("x"));
    let zero = p.var(Var::Int(APInt::from_i64(0)));
    p.add(Math::Add([x, zero]));
    p
}

#[test]
fn integer_literal_matches_and_binds_siblings() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let z = num(&mut g, 0);
    let root = add(&mut g, a, z);

    let matches = add_zero_pattern().search(&g);
    assert_eq!(matches.len(), 1);
    assert_eq!(g.find(matches[0].root), g.find(root));
    assert_eq!(matches[0].subst.get(&Var::Symbol("x")), Some(g.find(a)));
}

#[test]
fn integer_literal_rejects_other_constant() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let one = num(&mut g, 1);
    add(&mut g, a, one);
    assert!(add_zero_pattern().search(&g).is_empty());
}

#[test]
fn float_literal_matches_constant() {
    let mut p: Pattern<Math, &'static str> = Pattern::new();
    p.var(Var::Float(APFloat::from_f64(2.5)));

    let mut g = EGraph::new();
    let c = fnum(&mut g, 2.5);
    fnum(&mut g, 1.0);

    let matches = p.search(&g);
    assert_eq!(matches.len(), 1);
    assert_eq!(g.find(matches[0].root), g.find(c));
}

/// Whether `class` holds a constant e-node equal to `node` (constants are
/// leaves, so operator equality via [`ENode::matches`] is full equality).
fn class_has_const(eg: &EGraph<Math>, node: Option<Math>, class: Id) -> bool {
    match node {
        Some(n) => eg.nodes(class).iter().any(|e| e.matches(&n)),
        None => false,
    }
}

/// Reference matcher: straightforward recursive enumeration to cross-check
/// [`Pattern::search`], reading the pattern through its public accessors.
fn brute_node(
    p: &Pattern<Math, &'static str>,
    eg: &EGraph<Math>,
    pat: Id,
    class: Id,
    partial: Substitution<&'static str>,
) -> Vec<Substitution<&'static str>> {
    match p.node(pat) {
        PatternNode::Var(var @ Var::Symbol(_)) => {
            let mut s = partial;
            match s.get(var) {
                Some(b) if eg.find(b) != eg.find(class) => vec![],
                Some(_) => vec![s],
                None => {
                    s.insert(var.clone(), eg.find(class));
                    vec![s]
                }
            }
        }
        PatternNode::Var(Var::Int(v)) => {
            if class_has_const(eg, Math::from_int(v.clone()), class) {
                vec![partial]
            } else {
                vec![]
            }
        }
        PatternNode::Var(Var::Float(v)) => {
            if class_has_const(eg, Math::from_float(v.clone()), class) {
                vec![partial]
            } else {
                vec![]
            }
        }
        PatternNode::Node(t) => {
            let mut out = Vec::new();
            for enode in eg.nodes(class) {
                if !t.matches(enode) || t.children().len() != enode.children().len() {
                    continue;
                }
                let mut parts = vec![partial.clone()];
                for (pc, ec) in t.children().iter().zip(enode.children()) {
                    let child = eg.find(*ec);
                    parts = parts
                        .into_iter()
                        .flat_map(|p2| brute_node(p, eg, *pc, child, p2))
                        .collect();
                }
                out.extend(parts);
            }
            out
        }
    }
}

/// `(root, sorted bindings)`, canonicalized for order-independent comparison.
type Hit = (Id, Vec<(Var<&'static str>, Id)>);

fn canonical_bindings(
    eg: &EGraph<Math>,
    s: &Substitution<&'static str>,
) -> Vec<(Var<&'static str>, Id)> {
    let mut v: Vec<_> = s
        .entries()
        .map(|(var, id)| (var.clone(), eg.find(id)))
        .collect();
    v.sort();
    v
}

fn brute(p: &Pattern<Math, &'static str>, eg: &EGraph<Math>) -> Vec<Hit> {
    let mut out = Vec::new();
    for class in eg.classes() {
        let root = eg.find(class.id());
        for s in brute_node(p, eg, p.root(), root, Substitution::new()) {
            out.push((root, canonical_bindings(eg, &s)));
        }
    }
    out.sort();
    out
}

fn via_search(p: &Pattern<Math, &'static str>, eg: &EGraph<Math>) -> Vec<Hit> {
    let mut out: Vec<_> = p
        .search(eg)
        .into_iter()
        .map(|m| (eg.find(m.root), canonical_bindings(eg, &m.subst)))
        .collect();
    out.sort();
    out
}

/// `search` must equal the brute-force set even under congruence and nested patterns.
#[test]
fn search_matches_brute_force_with_congruence() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c = sym(&mut g, 2);
    let z = num(&mut g, 0);
    // Merge two distinct adds so a class holds multiple Add e-nodes and the index carries an absorbed id.
    let ab = add(&mut g, a, b);
    let ba = add(&mut g, b, a);
    let abz = add(&mut g, ab, z);
    let _nested = add(&mut g, a, ab);
    let _nested2 = add(&mut g, c, ba);
    let nn = neg(&mut g, a);
    let _nnn = neg(&mut g, nn);
    g.union(ab, ba);
    g.union(abz, c);
    g.rebuild();

    let bare = {
        let mut p = Pattern::new();
        p.var(Var::Symbol("x"));
        p
    };
    let two = add_pattern();
    let nested = {
        let mut p = Pattern::new();
        let x = p.var(Var::Symbol("x"));
        let y = p.var(Var::Symbol("y"));
        let zz = p.var(Var::Symbol("z"));
        let inner = p.add(Math::Add([y, zz]));
        p.add(Math::Add([x, inner]));
        p
    };
    let nonlinear = {
        let mut p = Pattern::new();
        let x = p.var(Var::Symbol("x"));
        p.add(Math::Add([x, x]));
        p
    };
    let dneg = {
        let mut p = Pattern::new();
        let x = p.var(Var::Symbol("x"));
        let inner = p.add(Math::Neg([x]));
        p.add(Math::Neg([inner]));
        p
    };
    for p in [&bare, &two, &nested, &nonlinear, &dneg, &add_zero_pattern()] {
        assert_eq!(via_search(p, &g), brute(p, &g));
    }
}

#[test]
fn instantiate_builds_literal_constants() {
    let mut g = EGraph::new();
    let five = num(&mut g, 5);
    let half = fnum(&mut g, 0.5);

    let mut int_pat: Pattern<Math, &'static str> = Pattern::new();
    int_pat.var(Var::Int(APInt::from_i64(5)));
    let built_int = int_pat.instantiate(&mut g, &Substitution::new());
    assert_eq!(g.find(built_int), g.find(five));

    let mut float_pat: Pattern<Math, &'static str> = Pattern::new();
    float_pat.var(Var::Float(APFloat::from_f64(0.5)));
    let built_float = float_pat.instantiate(&mut g, &Substitution::new());
    assert_eq!(g.find(built_float), g.find(half));
}

#[test]
fn int_leaf_matches_an_assumed_constant_under_the_scope() {
    let mut g = EGraph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let sum = add(&mut g, a, b);
    g.rebuild();

    let mut pattern: Pattern<Math, &'static str> = Pattern::new();
    let x = pattern.var(Var::Symbol("x"));
    let zero = pattern.var(Var::Int(APInt::from_i64(0)));
    pattern.add(Math::Add([x, zero]));

    assert!(pattern.search(&g).is_empty());
    g.push_context();
    g.assume_const(b, Math::Num(0));
    let matches = pattern.search(&g);
    assert_eq!(matches.len(), 1);
    assert_eq!(g.find(matches[0].root), g.find(sum));
    g.pop_context();
    assert!(pattern.search(&g).is_empty());
}

#[test]
fn height_counts_template_levels_below_the_root() {
    let mut p: Pattern<Math, &'static str> = Pattern::new();
    let x = p.var(Var::Symbol("x"));
    assert_eq!(p.height(), 0);

    let leaf = p.add(Math::Num(0));
    assert_eq!(p.height(), 0);

    let inner = p.add(Math::Add([x, leaf]));
    assert_eq!(p.height(), 1);

    // A DAG-shared child counts once, at its own depth.
    p.add(Math::Add([inner, inner]));
    assert_eq!(p.height(), 2);

    p.set_root(x);
    assert_eq!(p.height(), 0);
}
