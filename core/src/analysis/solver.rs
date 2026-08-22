//! Generic monotone fixpoint solver.
//!
//! An analysis states a [`Lattice`] of facts, the nodes it holds them about,
//! and how a fact rising at one node raises the facts of the nodes it feeds
//! ([`FactDomain`]). [`solve`] runs that statement to fixpoint over a worklist,
//! so an analysis is a lattice definition rather than another IR traversal.
//!
//! Facts are keyed in a hash map but never iterated: iteration order comes from
//! the worklist, which is a `Vec` seeded in IR order, so a solution is
//! reproducible bit for bit.

use std::collections::HashMap;
use std::hash::Hash;

/// A join-semilattice of facts. [`Lattice::bottom`] is "nothing known yet" and
/// must be the identity of [`Lattice::join`]; `join` must be the least upper
/// bound of its arguments, and the lattice must have no infinite ascending
/// chain, or [`solve`] does not terminate.
pub trait Lattice: Clone + PartialEq {
    fn bottom() -> Self;
    fn join(&self, other: &Self) -> Self;
}

/// One analysis: what it knows before iteration starts, and how a raised fact
/// propagates.
pub trait FactDomain {
    type Node: Copy + Eq + Hash;
    type Fact: Lattice;

    /// Raise the facts that hold before any transfer runs.
    fn seed(&self, facts: &mut Facts<Self::Node, Self::Fact>);

    /// `node`'s fact just rose: raise the facts of everything it feeds.
    fn transfer(&self, node: Self::Node, facts: &mut Facts<Self::Node, Self::Fact>);
}

/// What the solver knows, and what it still has to look at.
pub struct Facts<N, F> {
    facts: HashMap<N, F>,
    worklist: Vec<N>,
}

impl<N, F> Default for Facts<N, F> {
    fn default() -> Self {
        Self {
            facts: HashMap::new(),
            worklist: Vec::new(),
        }
    }
}

impl<N: Copy + Eq + Hash, F: Lattice> Facts<N, F> {
    /// What is known about `node`, [`Lattice::bottom`] until something is.
    pub fn get(&self, node: N) -> F {
        self.facts.get(&node).cloned().unwrap_or_else(F::bottom)
    }

    /// Join `fact` into what `node` holds. A node whose fact rises is queued for
    /// its transfer to run again.
    pub fn raise(&mut self, node: N, fact: F) {
        let current = self.get(node);
        let joined = current.join(&fact);
        if joined != current {
            self.facts.insert(node, joined);
            self.worklist.push(node);
        }
    }
}

/// Run `domain` to fixpoint.
pub fn solve<D: FactDomain>(domain: &D) -> Facts<D::Node, D::Fact> {
    let mut facts = Facts::default();
    domain.seed(&mut facts);
    while let Some(node) = facts.worklist.pop() {
        domain.transfer(node, &mut facts);
    }
    facts
}
