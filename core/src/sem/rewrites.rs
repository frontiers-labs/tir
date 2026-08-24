//! The proved algebraic rewrites the semantic e-graph saturates with, plus the
//! small saturation driver over the [`tir_symbolic`] e-graph.
//!
//! Instruction selection saturates a whole function's e-graph with them before
//! covering.

use std::collections::HashSet;

use tir_symbolic::egraph::{EMatch, ENode, Id, Pattern};

use super::egraph::SemEGraph;
use super::node::SemNode;
use super::theory::axioms;
use crate::Context;

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

/// Saturate an open scope's assumption over `roots`, the base graph already being
/// saturated globally. `roots` are searched verbatim — the caller has already
/// narrowed them to the classes the scope changed, and a class it left alone is
/// at the base fixpoint. Each round re-narrows, since applying a rewrite mints
/// classes and those are changed by construction.
pub fn saturate_scope(
    ctx: &Context,
    eg: &mut SemEGraph,
    rewrites: &[IselRewrite],
    limits: SaturationLimits,
    roots: Vec<Id>,
) {
    saturate_impl(ctx, eg, rewrites, limits, Some(roots));
}

fn saturate_impl(
    ctx: &Context,
    eg: &mut SemEGraph,
    rewrites: &[IselRewrite],
    limits: SaturationLimits,
    mut roots: Option<Vec<Id>>,
) {
    let search = |eg: &SemEGraph, rewrite: &IselRewrite, roots: &Option<Vec<Id>>| match roots {
        Some(roots) => rewrite.searcher.search_roots(eg, roots.iter().copied()),
        None => rewrite.searcher.search(eg),
    };
    for _ in 0..limits.max_iterations {
        let mut matches = Vec::new();
        for (index, rw) in rewrites.iter().enumerate() {
            if rw.post_saturation {
                continue;
            }
            for m in search(eg, rw, &roots) {
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
            let dirty: HashSet<Id> = eg.scope_dirty().into_iter().collect();
            *roots = reachable_roots(eg, std::mem::take(roots))
                .into_iter()
                .filter(|class| dirty.contains(class))
                .collect();
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
            search(eg, rewrite, &roots)
                .into_iter()
                .map(move |matched| (index, matched))
        })
        .collect();
    for (index, matched) in &matches {
        (rewrites[*index].apply)(ctx, eg, matched);
    }
    eg.rebuild();
}

/// Discovery order is deterministic (DFS from `roots` in the given order):
/// callers iterate the result into searching and application, where order
/// decides which node wins a cost tie downstream.
pub(crate) fn reachable_roots(eg: &SemEGraph, roots: impl IntoIterator<Item = Id>) -> Vec<Id> {
    let mut seen = HashSet::new();
    let mut reachable = Vec::new();
    let mut pending: Vec<_> = roots.into_iter().collect();
    while let Some(root) = pending.pop() {
        let root = eg.find(root);
        if !seen.insert(root) {
            continue;
        }
        reachable.push(root);
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
