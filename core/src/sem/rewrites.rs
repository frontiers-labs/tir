//! The proved algebraic rewrites the semantic e-graph saturates with, plus the
//! small saturation driver over the [`tir_symbolic`] e-graph.
//!
//! Instruction selection saturates a whole function's e-graph with them before
//! covering.

use std::collections::HashSet;

use tir_relational::Rule;
use tir_symbolic::egraph::{Delta, ENode, Id, RoundStats, Timer, trace_enabled};

use super::SymKind;
use super::axioms::{Axiom, Folding, Interpretation};
use super::egraph::SemEGraph;
use super::node::SemNode;
use super::theory::axioms;
use crate::Context;

/// The axioms a target selects with and the rules they compile to. The axioms
/// stay because a proof obligation names the one it belongs to: `TIR_VERIFY_AXIOMS`
/// discharges each width instantiation as it fires.
#[derive(Default)]
pub struct Theory {
    pub rules: Vec<Rule<SemNode>>,
    axioms: Vec<Axiom>,
    /// The pure ops the heads fold over constants; an extern id names one.
    folds: Vec<SymKind>,
}

impl Theory {
    /// Add an axiom's rules: the reading that folds an operand the graph turns
    /// out to have made constant, where that is a different rule at all, and the
    /// one that does not.
    pub fn push(&mut self, axiom: Axiom) {
        let index = self.axioms.len();
        if let Some((folding, true)) = axiom.compile(index, &mut self.folds, Folding::Assume) {
            self.rules.push(folding);
        }
        if let Some((plain, _)) = axiom.compile(index, &mut self.folds, Folding::Never) {
            self.rules.push(plain);
        }
        self.axioms.push(axiom);
    }

    /// Add a rule that is not an axiom — a target's own bridge, or a test's.
    pub fn push_rule(&mut self, rule: Rule<SemNode>) {
        self.rules.push(rule);
    }

    /// Whether any axiom decomposes a wide constant in place.
    pub fn materializes_constants(&self) -> bool {
        self.axioms.iter().any(Axiom::materializes_constants)
    }

    fn interpretation<'a>(&'a self, context: &'a Context) -> Interpretation<'a> {
        Interpretation::new(context, &self.axioms, &self.folds)
    }
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

/// Saturate `eg` with `theory`. Each iteration searches every rule against the
/// same snapshot, applies all matches, then rebuilds — so a node born this
/// iteration is only visible to the next. Stops at a fixpoint (an iteration that
/// changes neither the class nor the node count nor a fact) or once a limit is
/// reached.
pub fn saturate(ctx: &Context, eg: &mut SemEGraph, theory: &Theory, limits: SaturationLimits) {
    saturate_impl(ctx, eg, theory, limits, None);
}

/// Saturate an open scope's assumption over `roots`, the base graph already being
/// saturated globally. `roots` are searched verbatim — the caller has already
/// narrowed them to the classes the scope changed, and a class it left alone is
/// at the base fixpoint. Each round re-narrows, since applying a rule mints
/// classes and those are changed by construction.
pub fn saturate_scope(
    ctx: &Context,
    eg: &mut SemEGraph,
    theory: &Theory,
    limits: SaturationLimits,
    roots: Vec<Id>,
) {
    saturate_impl(ctx, eg, theory, limits, Some(roots));
}

fn saturate_impl(
    ctx: &Context,
    eg: &mut SemEGraph,
    theory: &Theory,
    limits: SaturationLimits,
    mut roots: Option<Vec<Id>>,
) {
    // Round 0 searches everything the caller asked for; from there on only the
    // classes the previous round changed, and their parents up to each rule's
    // height, can hold a match the round before did not already apply.
    let timer = Timer::start();
    let externs = theory.interpretation(ctx);
    eg.take_changed();
    let mut delta: Option<Delta> = None;
    // Cleared by every exit that reached a fixpoint; a stop on a limit leaves it
    // set, since the matches such a stop never reached are not in the change log
    // and the next saturation of this graph may not trust it.
    let mut on_a_limit = true;
    for _ in 0..limits.max_iterations {
        let mut stats = RoundStats::start(eg, delta.as_ref());
        let mut matches = Vec::new();
        for (index, rule) in theory.rules.iter().enumerate() {
            if rule.post_saturation {
                continue;
            }
            let round = round_roots(eg, rule, roots.as_deref(), delta.as_mut());
            stats.searched(round.len(), delta.as_ref());
            let narrow = delta.is_some() && !rule.plan.unbounded();
            for m in rule.plan.search(eg, round, &|_, _| true, narrow, &externs) {
                matches.push((index, m));
            }
        }
        if matches.is_empty() {
            stats.finish(eg);
            on_a_limit = false;
            break;
        }

        let before = (eg.num_classes(), eg.total_size(), eg.stats().raises);
        for (index, m) in &matches {
            if trace_enabled() {
                eprintln!(
                    "M {} {}",
                    theory.rules[*index].name,
                    eg.find(m.root).index()
                );
            }
            stats.apply(eg, |eg| eg.apply_head(&theory.rules[*index].head, m));
        }
        eg.rebuild();
        stats.finish(eg);
        delta = eg.take_changed().map(Delta::new);
        if let Some(roots) = &mut roots {
            let dirty: HashSet<Id> = eg.scope_dirty().into_iter().collect();
            *roots = reachable_roots(eg, std::mem::take(roots))
                .into_iter()
                .filter(|class| dirty.contains(class))
                .collect();
        }

        if (eg.num_classes(), eg.total_size(), eg.stats().raises) == before {
            // The counts held, but a round that changed only facts changed
            // nothing they count, and is not a fixpoint. `None` is the widest
            // such log there is, so it counts too.
            on_a_limit = delta.as_ref().is_none_or(|delta| !delta.is_empty());
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

    let matches: Vec<_> = theory
        .rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.post_saturation)
        .flat_map(|(index, rule)| {
            let roots = round_roots(eg, rule, roots.as_deref(), None);
            rule.plan
                .search(eg, roots, &|_, _| true, false, &externs)
                .into_iter()
                .map(move |matched| (index, matched))
        })
        .collect();
    for (index, matched) in &matches {
        eg.apply_head(&theory.rules[*index].head, matched);
    }
    eg.rebuild();
    timer.finish();
}

/// The classes a round searches `rule` at: what the caller narrowed the
/// saturation to, else everything the rule's root atom can match, and then only
/// the frontier at the rule's height — for a rule the change log can speak for.
fn round_roots(
    eg: &SemEGraph,
    rule: &Rule<SemNode>,
    scope: Option<&[Id]>,
    delta: Option<&mut Delta>,
) -> Vec<Id> {
    let mut roots = match scope {
        Some(scope) => scope.to_vec(),
        None => rule.plan.roots(eg),
    };
    if let Some(delta) = delta.filter(|_| !rule.plan.unbounded()) {
        let frontier = delta.at(eg, rule.plan.height());
        roots.retain(|&root| frontier.binary_search(&eg.find(root)).is_ok());
    }
    roots
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
pub fn discover_rewrites() -> Theory {
    let mut theory = Theory::default();
    for axiom in axioms() {
        theory.push(axiom);
    }
    theory
}
