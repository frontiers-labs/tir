use crate::egraph::{EGraph, EMatch, ENode, Id, Pattern, Substitution};

/// Imperative RHS: given the e-graph, match bindings, and matched root, assert the equivalences the rewrite proves.
pub type Applier<N, S> = dyn Fn(&mut EGraph<N>, &Substitution<S>, Id) + Send + Sync;

/// The right-hand side of a [`Rewrite`].
pub enum Rhs<N: ENode, S> {
    /// A template instantiated from the match and unioned with the matched root.
    Pattern(Pattern<N, S>),
    /// An arbitrary applier for rewrites a template cannot express.
    Apply(Box<Applier<N, S>>),
}

/// Search the e-graph for `lhs`, then apply `rhs` to each match.
pub struct Rewrite<N: ENode, S> {
    pub name: String,
    pub lhs: Pattern<N, S>,
    pub rhs: Rhs<N, S>,
}

impl<N: ENode, S: Clone + PartialEq> Rewrite<N, S> {
    pub fn new(name: impl Into<String>, lhs: Pattern<N, S>, rhs: Rhs<N, S>) -> Self {
        Self {
            name: name.into(),
            lhs,
            rhs,
        }
    }

    /// Apply the right-hand side to a single match.
    pub fn apply_match(&self, eg: &mut EGraph<N>, m: &EMatch<S>) {
        match &self.rhs {
            Rhs::Pattern(p) => {
                let id = p.instantiate(eg, &m.subst);
                eg.union(m.root, id);
            }
            Rhs::Apply(f) => f(eg, &m.subst, m.root),
        }
    }

    /// One pass: apply the rewrite to every current match, then restore congruence.
    pub fn apply_all(&self, eg: &mut EGraph<N>) {
        for m in self.lhs.search(eg) {
            self.apply_match(eg, &m);
        }
        eg.rebuild();
    }
}
