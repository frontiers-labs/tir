//! The proved algebraic rewrites the semantic e-graph saturates with, plus the
//! small saturation driver over the [`tir_symbolic`] e-graph.
//!
//! Instruction selection saturates a whole function's e-graph with them before
//! covering.

use std::collections::HashSet;

use tir_symbolic::egraph::{Delta, EMatch, ENode, Id, Pattern, RoundStats, Timer};

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
    /// Whether a saturation round may narrow this rule's roots to the change
    /// frontier. Sound only when `apply` reads no class the `searcher` did not
    /// bind, so that the pattern's height bounds everything the rule looks at.
    /// [`Axiom::compile`](super::theory::Axiom) sets it, because it generates an
    /// applier that only ever reads `m.binding(..)`; a hand-written one must
    /// leave it false and be searched everywhere, every round.
    pub cone_bounded: bool,
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
    // Round 0 searches everything the caller asked for; from there on only the
    // classes the previous round changed, and their parents up to each pattern's
    // height, can hold a match the round before did not already apply.
    let timer = Timer::start();
    eg.take_changed();
    let mut delta: Option<Delta> = None;
    // Cleared by every exit that reached a fixpoint; a stop on a limit leaves it
    // set, since the matches such a stop never reached are not in the change log
    // and the next saturation of this graph may not trust it.
    let mut on_a_limit = true;
    for _ in 0..limits.max_iterations {
        let mut stats = RoundStats::start(delta.as_ref());
        let mut matches = Vec::new();
        for (index, rw) in rewrites.iter().enumerate() {
            if rw.post_saturation {
                continue;
            }
            let frontier = delta.as_mut().filter(|_| rw.cone_bounded);
            let round = rw.searcher.round_roots(eg, roots.as_deref(), frontier);
            stats.searched(round.len(), delta.as_ref());
            for m in rw.searcher.search_roots(eg, round) {
                matches.push((index, m));
            }
        }
        if matches.is_empty() {
            stats.finish();
            on_a_limit = false;
            break;
        }

        let before = (eg.num_classes(), eg.total_size());
        for (index, m) in &matches {
            stats.apply(eg, |eg| (rewrites[*index].apply)(ctx, eg, m));
        }
        eg.rebuild();
        stats.finish();
        delta = eg.take_changed().map(Delta::new);
        if let Some(roots) = &mut roots {
            let dirty: HashSet<Id> = eg.scope_dirty().into_iter().collect();
            *roots = reachable_roots(eg, std::mem::take(roots))
                .into_iter()
                .filter(|class| dirty.contains(class))
                .collect();
        }

        if (eg.num_classes(), eg.total_size()) == before {
            on_a_limit = false;
            break;
        }
        if eg.num_classes() >= limits.max_classes {
            break;
        }
    }
    if on_a_limit {
        eg.mark_all_changed();
    }
    eg.rebuild();

    let matches: Vec<_> = rewrites
        .iter()
        .enumerate()
        .filter(|(_, rewrite)| rewrite.post_saturation)
        .flat_map(|(index, rewrite)| {
            let roots = rewrite.searcher.round_roots(eg, roots.as_deref(), None);
            rewrite
                .searcher
                .search_roots(eg, roots)
                .into_iter()
                .map(move |matched| (index, matched))
        })
        .collect();
    for (index, matched) in &matches {
        (rewrites[*index].apply)(ctx, eg, matched);
    }
    eg.rebuild();
    timer.finish();
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
