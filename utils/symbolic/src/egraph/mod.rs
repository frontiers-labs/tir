//! Equality saturation over [`tir_relational`]'s columnar e-graph.
//!
//! The e-graph itself — hash-consing, congruence closure, scopes, the change log
//! — lives in `tir-relational`, which knows nothing about terms beyond the
//! [`ENode`] contract. What stays here is what saturation adds: patterns, the
//! matcher, rewrites, extraction and the round counters.

mod extract;
mod telemetry;

use std::ops::{Deref, DerefMut};

pub use tir_relational::trace_enabled;
pub use tir_relational::{ClassId as Id, ClassRef as EClass, Label as ENode};

pub use extract::*;
pub use telemetry::{RoundStats, Timer, report_saturation};

/// Per-round frontier of semi-naive saturation: the change log of the previous
/// round closed upward, cached by the pattern heights a rule set asks for.
pub struct Delta {
    /// `levels[h]` is Δ_h; grown on demand, each from the one below it.
    levels: Vec<Vec<Id>>,
}

impl Delta {
    /// Seed from a [`EGraph::take_changed`] drain.
    pub fn new(changed: Vec<Id>) -> Self {
        Self {
            levels: vec![changed],
        }
    }

    /// Nothing changed, so no rule can match anywhere new.
    pub fn is_empty(&self) -> bool {
        self.levels[0].is_empty()
    }

    /// Size of the change log this frontier grew from.
    pub fn len(&self) -> usize {
        self.levels[0].len()
    }

    /// Size of the deepest frontier asked for so far — levels only grow, so this
    /// is the widest set any rule searched.
    pub fn frontier(&self) -> usize {
        self.levels.last().expect("seeded level").len()
    }

    /// Δ_height, ascending.
    pub fn at<L: ENode>(&mut self, eg: &EGraph<L>, height: usize) -> &[Id] {
        while self.levels.len() <= height {
            let below = self.levels.last().expect("seeded level");
            self.levels.push(eg.delta(below, 1));
        }
        &self.levels[height]
    }
}

/// The saturating e-graph: [`tir_relational::Engine`] plus a rule driver.
pub struct EGraph<L: ENode>(tir_relational::Engine<L>);

impl<L: ENode> Default for EGraph<L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L: ENode> Deref for EGraph<L> {
    type Target = tir_relational::Engine<L>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<L: ENode> DerefMut for EGraph<L> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<L: ENode> EGraph<L> {
    pub fn new() -> Self {
        Self(tir_relational::Engine::new())
    }

    /// Saturate in place with `rules`, and the host functions their guards call.
    /// Each iteration searches every rule against one snapshot, then applies and
    /// rebuilds — a node born this iteration is visible only to the next. Stops
    /// at a fixpoint (nothing the class and node counts or the fact columns see
    /// changed, or an empty change log) or at a limit.
    pub fn saturate_rules(
        &mut self,
        rules: &[tir_relational::Rule<L>],
        externs: &dyn tir_relational::Externs<L>,
        iter_limit: usize,
        node_limit: usize,
    ) {
        let timer = Timer::start();
        let mut delta = self.take_changed().map(Delta::new);
        let mut iters = 0;
        loop {
            let size = self.total_size();
            if iters >= iter_limit || size >= node_limit {
                // Not a fixpoint: the matches this stop left unreached are not
                // in the change log, so the next saturation may not trust it.
                self.mark_all_changed();
                break;
            }
            let before = (self.num_classes(), size, self.stats().raises);

            let mut stats = RoundStats::start(self, delta.as_ref());
            let mut found: Vec<(&tir_relational::Rule<L>, Vec<tir_relational::Match>)> = Vec::new();
            for rule in rules {
                // Everything a rule reads is an atom or a guard over what an atom
                // bound, so both narrowings come free of any hand-asserted
                // licence — for a rule the change log can speak for. It cannot
                // speak for one whose match depends on rows outside the root's
                // cone.
                let bounded = !rule.plan.unbounded();
                let mut roots = rule.plan.roots(self);
                if let Some(delta) = delta.as_mut().filter(|_| bounded) {
                    let frontier = delta.at(self, rule.plan.height());
                    roots.retain(|&root| frontier.binary_search(&self.find(root)).is_ok());
                }
                stats.searched(roots.len(), delta.as_ref());
                let matches = rule.plan.search(
                    self,
                    roots,
                    &|_, _| true,
                    delta.is_some() && bounded,
                    externs,
                );
                found.push((rule, matches));
            }
            for (rule, matches) in &found {
                for m in matches {
                    if trace_enabled() {
                        eprintln!("M {} {}", rule.name, self.find(m.root).index());
                    }
                    stats.apply(self, |eg| eg.apply_head(&rule.head, rule.head_vars, m));
                }
            }
            self.rebuild();
            stats.finish(self);

            iters += 1;
            delta = self.take_changed().map(Delta::new);
            if delta.as_ref().is_some_and(Delta::is_empty) {
                break;
            }
            if (self.num_classes(), self.total_size(), self.stats().raises) == before {
                // The counts held, but the matches a stop never reached are not
                // named by a log this break is about to drop. `None` is the
                // widest such log there is, so it marks too.
                if delta.as_ref().is_none_or(|delta| !delta.is_empty()) {
                    self.mark_all_changed();
                }
                break;
            }
        }
        timer.finish();
    }
}
