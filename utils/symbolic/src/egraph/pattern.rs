use tir_adt::{APFloat, APInt};

use crate::egraph::{EGraph, ENode, Id};

#[derive(Debug, Clone, PartialEq, PartialOrd, Ord, Eq, Hash)]
pub enum Var<S> {
    Symbol(S),
    Int(APInt),
    Float(APFloat),
}

/// A mapping from pattern variables to the e-classes they bound to during a match.
#[derive(Debug, Clone, Eq, PartialEq, Ord, Hash, PartialOrd)]
pub struct Substitution<S> {
    pub(crate) vec: Vec<(Var<S>, Id)>,
}

impl<S> Default for Substitution<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Substitution<S> {
    pub fn new() -> Self {
        Self { vec: Vec::new() }
    }
}

impl<S: PartialEq> Substitution<S> {
    pub fn insert(&mut self, var: Var<S>, id: Id) -> Option<Id> {
        for pair in &mut self.vec {
            if var == pair.0 {
                return Some(core::mem::replace(&mut pair.1, id));
            }
        }
        self.vec.push((var, id));
        None
    }

    pub fn get(&self, var: &Var<S>) -> Option<Id> {
        self.vec
            .iter()
            .find(|pair| &pair.0 == var)
            .map(|pair| pair.1)
    }
}

/// One node of a [`Pattern`]: a template operator or a hole.
#[derive(Debug, Clone)]
pub enum PatternNode<N: ENode, S> {
    /// Template e-node; child ids are pattern-local indices into `nodes`, not e-class ids.
    Node(N),
    /// Leaf hole: `Symbol` binds any class; `Int`/`Float` match that constant unbound.
    Var(Var<S>),
}

/// Structural pattern over `N`: search template (LHS) and, via [`Self::instantiate`], rewrite RHS. Nodes stored bottom-up, so a child's index is always smaller than its parent's.
#[derive(Debug, Clone)]
pub struct Pattern<N: ENode, S> {
    nodes: Vec<PatternNode<N, S>>,
    root: Id,
}

/// One match of a [`Pattern`] against an e-graph: the matched e-class and the
/// variable bindings that made it match.
#[derive(Debug, Clone)]
pub struct EMatch<S> {
    pub root: Id,
    pub subst: Substitution<S>,
}

impl<N: ENode, S> Default for Pattern<N, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N: ENode, S> Pattern<N, S> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            root: Id::from_raw(0),
        }
    }

    /// Add a hole; its id is the root until a later `add`/`var` or [`Self::set_root`].
    pub fn var(&mut self, var: Var<S>) -> Id {
        self.push(PatternNode::Var(var))
    }

    /// Add a template node. Wire its children to ids returned by earlier calls.
    pub fn add(&mut self, node: N) -> Id {
        self.push(PatternNode::Node(node))
    }

    pub fn set_root(&mut self, root: Id) {
        self.root = root;
    }

    fn push(&mut self, node: PatternNode<N, S>) -> Id {
        let id = Id::from_raw(self.nodes.len() as u32);
        self.nodes.push(node);
        self.root = id;
        id
    }
}

impl<N: ENode, S: Clone + PartialEq> Pattern<N, S> {
    /// Every match across the e-graph; an operator-rooted pattern visits only classes holding that operator ([`EGraph::classes_with_op`]), a bare-variable root every class.
    pub fn search<'p>(&'p self, eg: &EGraph<N>) -> Vec<EMatch<S>> {
        let mut out = Vec::new();
        let mut goals: Vec<(Id, Id)> = Vec::new();
        // Bindings borrow the pattern's `Var`s; names are cloned only when a full match is emitted.
        let mut subst: Vec<(&'p Var<S>, Id)> = Vec::new();
        let roots: Vec<Id> = match &self.nodes[self.root.index()] {
            PatternNode::Node(template) => eg.classes_with_op(template.op_key()),
            _ => eg.classes().map(|c| c.id()).collect(),
        };
        for root in roots {
            goals.push((self.root, root));
            self.solve(eg, root, &mut goals, &mut subst, &mut out);
            goals.clear();
        }
        out
    }

    /// Depth-first backtracking e-matcher: pops one goal off the `(pattern node, e-class)` stack, explores every solution restoring `goals`/`subst` between branches, then restores the goal for the caller.
    fn solve<'p>(
        &'p self,
        eg: &EGraph<N>,
        root: Id,
        goals: &mut Vec<(Id, Id)>,
        subst: &mut Vec<(&'p Var<S>, Id)>,
        out: &mut Vec<EMatch<S>>,
    ) {
        let Some((pat, class)) = goals.pop() else {
            out.push(EMatch {
                root,
                subst: Substitution {
                    vec: subst.iter().map(|&(v, id)| (v.clone(), id)).collect(),
                },
            });
            return;
        };
        let mark = goals.len();
        match &self.nodes[pat.index()] {
            PatternNode::Var(var @ Var::Symbol(_)) => {
                match subst.iter().find(|(v, _)| *v == var).map(|&(_, id)| id) {
                    Some(bound) if eg.find(bound) != eg.find(class) => {}
                    Some(_) => self.solve(eg, root, goals, subst, out),
                    None => {
                        subst.push((var, class));
                        self.solve(eg, root, goals, subst, out);
                        subst.pop();
                    }
                }
            }
            PatternNode::Var(Var::Int(v)) => {
                if class_has_const(eg, N::from_int(v.clone()), class) {
                    self.solve(eg, root, goals, subst, out);
                }
            }
            PatternNode::Var(Var::Float(v)) => {
                if class_has_const(eg, N::from_float(v.clone()), class) {
                    self.solve(eg, root, goals, subst, out);
                }
            }
            PatternNode::Node(template) => {
                let tchildren = template.children();
                for enode in eg.nodes(class) {
                    if !template.matches(enode) || tchildren.len() != enode.children().len() {
                        continue;
                    }
                    for (pc, ec) in tchildren.iter().zip(enode.children()).rev() {
                        goals.push((*pc, eg.find(*ec)));
                    }
                    self.solve(eg, root, goals, subst, out);
                    goals.truncate(mark);
                }
            }
        }
        goals.push((pat, class));
    }

    /// Build this pattern into `eg` under `subst`, returning the root e-class.
    pub fn instantiate(&self, eg: &mut EGraph<N>, subst: &Substitution<S>) -> Id {
        let mut ids: Vec<Id> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let id = match node {
                PatternNode::Var(var @ Var::Symbol(_)) => {
                    subst.get(var).expect("unbound pattern variable")
                }
                PatternNode::Var(Var::Int(v)) => {
                    let node = N::from_int(v.clone()).expect("language has no integer constants");
                    eg.add(node)
                }
                PatternNode::Var(Var::Float(v)) => {
                    let node = N::from_float(v.clone()).expect("language has no float constants");
                    eg.add(node)
                }
                PatternNode::Node(template) => {
                    let mut node = template.clone();
                    for child in node.children_mut() {
                        *child = ids[child.index()];
                    }
                    eg.add(node)
                }
            };
            ids.push(id);
        }
        ids[self.root.index()]
    }
}

/// Whether `class` holds constant `target` (a childless leaf); `false` if `target` is `None`.
fn class_has_const<N: ENode>(eg: &EGraph<N>, target: Option<N>, class: Id) -> bool {
    let Some(target) = target else {
        return false;
    };
    eg.nodes(class)
        .iter()
        .any(|n| n.children().is_empty() && target.matches(n))
}

#[cfg(test)]
mod tests {
    use tir_adt::{APFloat, APInt};

    use super::super::test_lang::*;
    use super::*;

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
            match self {
                Wild::Leaf(s) => *s as u64,
                Wild::Op(tag, _) => 1 << 32 | *tag as u64,
            }
        }
        fn op_key(&self) -> u64 {
            match self {
                Wild::Op(..) => 1 << 32,
                Wild::Leaf(_) => self.hash_cons(),
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

    /// Reference matcher: straightforward recursive enumeration to cross-check [`Pattern::search`].
    fn brute_node(
        p: &Pattern<Math, &'static str>,
        eg: &EGraph<Math>,
        pat: Id,
        class: Id,
        partial: Substitution<&'static str>,
    ) -> Vec<Substitution<&'static str>> {
        match &p.nodes[pat.index()] {
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

    fn brute(p: &Pattern<Math, &'static str>, eg: &EGraph<Math>) -> Vec<Hit> {
        let mut out = Vec::new();
        for class in eg.classes() {
            let root = eg.find(class.id());
            for s in brute_node(p, eg, p.root, root, Substitution::new()) {
                let mut v: Vec<_> = s.vec.into_iter().map(|(k, id)| (k, eg.find(id))).collect();
                v.sort();
                out.push((root, v));
            }
        }
        out.sort();
        out
    }

    fn via_search(p: &Pattern<Math, &'static str>, eg: &EGraph<Math>) -> Vec<Hit> {
        let mut out: Vec<_> = p
            .search(eg)
            .into_iter()
            .map(|m| {
                let mut v: Vec<_> = m
                    .subst
                    .vec
                    .into_iter()
                    .map(|(k, id)| (k, eg.find(id)))
                    .collect();
                v.sort();
                (eg.find(m.root), v)
            })
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
}
