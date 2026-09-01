//! The proved algebraic rewrites the semantic e-graph saturates with, plus the
//! small saturation driver over the [`tir_relational`] engine.
//!
//! Instruction selection saturates a whole function's e-graph with them before
//! covering.

use tir_relational::Rule;
use tir_relational::{
    ClassId as Id, Delta, RoundStats, Timer, round_roots as delta_roots, trace_enabled,
};

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
///
/// Round 0 starts from the change log rather than from the whole graph, so an
/// assumption scope pays for what it assumed instead of for the graph it assumed
/// it over. That is sound because the log is drained at the end of every
/// saturation that reaches a fixpoint (below), which leaves a scope's entry log
/// holding its own assertion alone, and because a scope saturates exactly once —
/// the log is a single consumable stream, so a second saturation under the same
/// assumption would find the assertion already gone. A stop on a limit marks
/// everything changed instead, so the next saturation is a full one.
pub fn saturate(ctx: &Context, eg: &mut SemEGraph, theory: &Theory, limits: SaturationLimits) {
    let timer = Timer::start();
    let externs = theory.interpretation(ctx);
    let mut log = eg.take_changed();
    // Everything the saturation made readable, for the post-saturation phase to
    // search. The rest of the graph was already at that phase's fixpoint when the
    // caller handed it over, so only these classes can hold a match it does not
    // have.
    let mut touched = log.clone();
    let mut delta: Option<Delta> = log.take().map(Delta::new);
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
            let round = round_roots(eg, rule, delta.as_mut());
            stats.searched(round.len(), delta.as_ref());
            if round.is_empty() {
                continue;
            }
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
            stats.apply(eg, |eg| {
                eg.apply_head(
                    &theory.rules[*index].head,
                    theory.rules[*index].head_vars,
                    m,
                )
            });
        }
        eg.rebuild();
        stats.finish(eg);
        let log = eg.take_changed();
        match (&mut touched, &log) {
            (Some(all), Some(changed)) => all.extend(changed.iter().copied()),
            _ => touched = None,
        }
        delta = log.map(Delta::new);
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
        touched = None;
    }
    eg.rebuild();

    let mut touched = touched.map(|mut all| {
        for id in &mut all {
            *id = eg.find(*id);
        }
        all.sort_unstable();
        all.dedup();
        Delta::new(all)
    });
    let matches: Vec<_> = theory
        .rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.post_saturation)
        .flat_map(|(index, rule)| {
            let roots = round_roots(eg, rule, touched.as_mut());
            let matches = if roots.is_empty() {
                Vec::new()
            } else {
                rule.plan.search(eg, roots, &|_, _| true, false, &externs)
            };
            matches.into_iter().map(move |matched| (index, matched))
        })
        .collect();
    for (index, matched) in &matches {
        eg.apply_head(
            &theory.rules[*index].head,
            theory.rules[*index].head_vars,
            matched,
        );
    }
    eg.rebuild();
    // A post-saturation head is terminal: the phase runs once, after the
    // fixpoint, and nothing feeds its results back. Draining the log says so, and
    // is what leaves the next assumption scope's entry log holding that scope's
    // own assertion rather than this fixpoint's tail. A limit stop is not a
    // fixpoint, so it keeps the "everything changed" mark instead.
    if !on_a_limit {
        eg.take_changed();
    }
    timer.finish();
}

/// The classes a round searches `rule` at: everything the rule's root atom can
/// match, and then only the frontier at the rule's height — for a rule the change
/// log can speak for.
fn round_roots(eg: &SemEGraph, rule: &Rule<SemNode>, delta: Option<&mut Delta>) -> Vec<Id> {
    match delta.filter(|_| !rule.plan.unbounded()) {
        Some(delta) => delta_roots(eg, &rule.plan, delta),
        None => rule.plan.roots(eg),
    }
}

/// The target-independent semantic invariants every rule set gets.
pub fn discover_rewrites() -> Theory {
    let mut theory = Theory::default();
    for axiom in axioms() {
        theory.push(axiom);
    }
    theory
}
