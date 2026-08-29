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
    cone_bounded: bool,
}

impl<N: ENode, S: Clone + PartialEq> Rewrite<N, S> {
    /// Whether a saturation round may narrow this rule's roots to the change
    /// frontier. Sound only when the pattern is the whole match predicate, so
    /// that [`Pattern::height`] bounds every class the rule reads. A template RHS
    /// gets it for free; an applier closure may decline a match on anything it
    /// likes — instcombine's memory laws walk address and state chains far below
    /// the class their pattern binds — so one is searched everywhere, every
    /// round, unless it claims otherwise via [`Self::reads_only_its_match`].
    pub fn cone_bounded(&self) -> bool {
        self.cone_bounded
    }

    /// Claim that this rule's applier reads no class more than
    /// [`Pattern::height`] parent-edges below the matched root, so a round may
    /// narrow its roots as if the RHS were a template. The bound is on *depth*,
    /// not on what the match bound: an applier may read the other nodes of the
    /// root's class and their operands, which no substitution names, because a
    /// change to any of them still lands the root in the frontier. Nothing checks
    /// this — an applier that reads deeper is silently starved of matches — so
    /// state the read depth where you call it.
    pub fn reads_only_its_match(mut self) -> Self {
        self.cone_bounded = true;
        self
    }

    pub fn new(name: impl Into<String>, lhs: Pattern<N, S>, rhs: Rhs<N, S>) -> Self {
        Self {
            name: name.into(),
            lhs,
            cone_bounded: matches!(rhs, Rhs::Pattern(_)),
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
