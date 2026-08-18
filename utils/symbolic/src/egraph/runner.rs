use crate::egraph::{EGraph, ENode, Id, Rewrite};

/// Drives equality saturation to a fixpoint or limit, then hands back the saturated [`EGraph`] and canonical roots.
pub struct Runner<N: ENode> {
    egraph: EGraph<N>,
    roots: Vec<Id>,
    iter_limit: usize,
    node_limit: usize,
}

impl<N: ENode> Runner<N> {
    pub fn new(egraph: EGraph<N>, roots: Vec<Id>) -> Self {
        Self {
            egraph,
            roots,
            iter_limit: 30,
            node_limit: 100_000,
        }
    }

    pub fn with_iter_limit(mut self, limit: usize) -> Self {
        self.iter_limit = limit;
        self
    }

    pub fn with_node_limit(mut self, limit: usize) -> Self {
        self.node_limit = limit;
        self
    }

    pub fn egraph(&self) -> &EGraph<N> {
        &self.egraph
    }

    /// The construction-time roots, canonicalized to their current classes.
    pub fn roots(&self) -> Vec<Id> {
        self.roots.iter().map(|&r| self.egraph.find(r)).collect()
    }

    /// Saturate with `rules`; each iteration searches against one snapshot, so a node born this iteration is visible only to the next. Stops at a fixpoint or the iter/node limit.
    pub fn run<'a, S>(&mut self, rules: impl IntoIterator<Item = &'a Rewrite<N, S>>)
    where
        N: 'a,
        S: Clone + PartialEq + 'a,
    {
        self.egraph
            .saturate(rules, self.iter_limit, self.node_limit);
    }
}
