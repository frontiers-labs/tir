//! The proved algebraic rewrites used to saturate the program e-graph before
//! covering, plus the small saturation driver over the [`tir_symbolic`] e-graph.

use std::collections::HashSet;

use tir::Context;
use tir_symbolic::egraph::{EMatch, ENode, Id, Pattern};

use super::node::{SemEGraph, SemNode};
use super::theory::axioms;

/// The right-hand side of an [`IselRewrite`]: given the e-graph and a match, assert
/// the proven equivalence (typically by building nodes and unioning the result with
/// the match root).
pub type IselApplier = dyn Fn(&Context, &mut SemEGraph, &EMatch<u32>) + Send + Sync;

/// An imperative algebraic rewrite: e-match `searcher`, then call `apply` for each
/// match to assert the proven equivalence.
pub struct IselRewrite {
    pub name: String,
    pub searcher: Pattern<SemNode, u32>,
    pub apply: Box<IselApplier>,
    /// Apply once after iterative saturation reaches a fixpoint.
    pub post_saturation: bool,
}

/// Saturation budget: a cap on iterations and on e-class count.
#[derive(Clone, Copy, Debug)]
pub struct SaturationLimits {
    pub max_iterations: usize,
    pub max_classes: usize,
}

impl Default for SaturationLimits {
    fn default() -> Self {
        Self {
            max_iterations: 30,
            max_classes: 10_000,
        }
    }
}

/// Saturate `eg` with `rewrites`. Each iteration searches every rewrite against the
/// same snapshot, applies all matches, then rebuilds — so a node born this iteration
/// is only visible to the next. Stops at a fixpoint (an iteration that changes
/// neither the class nor the node count) or once a limit is reached.
pub fn saturate(
    ctx: &Context,
    eg: &mut SemEGraph,
    rewrites: &[IselRewrite],
    limits: SaturationLimits,
) {
    saturate_impl(ctx, eg, rewrites, limits, None);
}

/// Saturate only expressions reachable from `roots`. Scoped assumptions use
/// this after the base graph has already been saturated globally.
pub fn saturate_roots(
    ctx: &Context,
    eg: &mut SemEGraph,
    rewrites: &[IselRewrite],
    limits: SaturationLimits,
    roots: impl IntoIterator<Item = Id>,
) {
    let roots = reachable_roots(eg, roots);
    saturate_impl(ctx, eg, rewrites, limits, Some(roots));
}

fn saturate_impl(
    ctx: &Context,
    eg: &mut SemEGraph,
    rewrites: &[IselRewrite],
    limits: SaturationLimits,
    mut roots: Option<HashSet<Id>>,
) {
    for _ in 0..limits.max_iterations {
        let mut matches = Vec::new();
        for (index, rw) in rewrites.iter().enumerate() {
            if rw.post_saturation {
                continue;
            }
            let found = match &roots {
                Some(roots) => rw.searcher.search_roots(eg, roots.iter().copied()),
                None => rw.searcher.search(eg),
            };
            for m in found {
                matches.push((index, m));
            }
        }
        if matches.is_empty() {
            break;
        }

        let before = (eg.num_classes(), eg.total_size());
        for (index, m) in &matches {
            (rewrites[*index].apply)(ctx, eg, m);
        }
        eg.rebuild();
        if let Some(roots) = &mut roots {
            *roots = reachable_roots(eg, std::mem::take(roots));
        }

        if (eg.num_classes(), eg.total_size()) == before || eg.num_classes() >= limits.max_classes {
            break;
        }
    }
    eg.rebuild();

    let matches: Vec<_> = rewrites
        .iter()
        .enumerate()
        .filter(|(_, rewrite)| rewrite.post_saturation)
        .flat_map(|(index, rewrite)| {
            let found = match &roots {
                Some(roots) => rewrite.searcher.search_roots(eg, roots.iter().copied()),
                None => rewrite.searcher.search(eg),
            };
            found.into_iter().map(move |matched| (index, matched))
        })
        .collect();
    for (index, matched) in &matches {
        (rewrites[*index].apply)(ctx, eg, matched);
    }
    eg.rebuild();
}

pub(crate) fn reachable_roots(eg: &SemEGraph, roots: impl IntoIterator<Item = Id>) -> HashSet<Id> {
    let mut reachable = HashSet::new();
    let mut pending: Vec<_> = roots.into_iter().collect();
    while let Some(root) = pending.pop() {
        let root = eg.find(root);
        if !reachable.insert(root) {
            continue;
        }
        for node in eg.nodes(root) {
            pending.extend(node.children().iter().map(|child| eg.find(*child)));
        }
    }
    reachable
}

/// The target-independent semantic invariants every rule set gets.
pub(crate) fn discover_rewrites() -> Vec<IselRewrite> {
    axioms().into_iter().map(|axiom| axiom.compile()).collect()
}
