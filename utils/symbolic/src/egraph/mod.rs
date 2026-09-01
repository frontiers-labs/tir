//! Equality saturation over [`tir_relational`]'s columnar e-graph.
//!
//! The e-graph itself — hash-consing, congruence closure, scopes, the change log
//! — lives in `tir-relational`, which knows nothing about terms beyond the
//! [`ENode`] contract. What stays here is what saturation adds: patterns, the
//! matcher, rewrites, extraction and the round counters.

mod extract;
mod telemetry;

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use tir_adt::FxBuildHasher;

pub use tir_relational::trace_enabled;
pub use tir_relational::{ClassId as Id, ClassRef as EClass, Label as ENode};

pub use extract::*;
pub use telemetry::{RoundStats, Timer, report_saturation};

/// Δ_h grouped by operator: for each, the classes holding it paired with the row
/// each one enters that operator's bucket at, which is what orders the group.
type OpGroups = HashMap<u64, Vec<(u32, Id)>, FxBuildHasher>;

/// Per-round frontier of semi-naive saturation: the change log of the previous
/// round closed upward, cached by the pattern heights a rule set asks for.
pub struct Delta {
    /// `levels[h]` is Δ_h; grown on demand, each from the one below it.
    levels: Vec<Vec<Id>>,
    /// `by_op[h]` groups Δ_h by the operators its classes hold, so a round scans
    /// each depth once instead of once per rule.
    /// Each group is ordered by the row a class enters the bucket at.
    by_op: Vec<OpGroups>,
}

impl Delta {
    /// Seed from a [`EGraph::take_changed`] drain.
    pub fn new(changed: Vec<Id>) -> Self {
        Self {
            levels: vec![changed],
            by_op: Vec::new(),
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

    /// The classes of Δ_height holding `op`, in the order the operator's row
    /// bucket holds them: where a rule of that height rooted on that operator can
    /// match anew.
    ///
    /// Scanning the frontier once per depth beats materializing each rule's whole
    /// bucket and filtering it, because a settled round's frontier is a handful of
    /// classes while the bucket holds every term of that shape in the function.
    /// The bucket is appended to as rows are minted, and a class enters it at its
    /// first row of that operator, so ordering each group by that row reproduces
    /// the bucket's order exactly — which is the root order a match's position in
    /// the round is defined by, and so the order class ids are assigned in.
    pub fn roots<L: ENode>(&mut self, eg: &EGraph<L>, height: usize, op: u64) -> &[(u32, Id)] {
        self.at(eg, height);
        while self.by_op.len() <= height {
            let mut by_op: OpGroups = HashMap::default();
            let mut ops: Vec<(u64, u32)> = Vec::new();
            for &class in &self.levels[self.by_op.len()] {
                ops.clear();
                for row in eg.rows(class) {
                    let op = eg.node(row).op_key();
                    match ops.iter_mut().find(|(seen, _)| *seen == op) {
                        Some((_, first)) => *first = (*first).min(row.0),
                        None => ops.push((op, row.0)),
                    }
                }
                for &(op, row) in &ops {
                    by_op.entry(op).or_default().push((row, class));
                }
            }
            for group in by_op.values_mut() {
                group.sort_unstable();
            }
            self.by_op.push(by_op);
        }
        self.by_op[height].get(&op).map_or(&[], Vec::as_slice)
    }
}

/// The classes a semi-naive round searches `plan` at: the change frontier at the
/// plan's height, restricted to the operator its root binds.
pub fn round_roots<L: ENode>(
    eg: &EGraph<L>,
    plan: &tir_relational::Plan<L>,
    delta: &mut Delta,
) -> Vec<Id> {
    match plan.root_op() {
        Some(op) => delta
            .roots(eg, plan.height(), op)
            .iter()
            .map(|&(_, class)| class)
            .collect(),
        None => delta.at(eg, plan.height()).to_vec(),
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
                let roots = match delta.as_mut().filter(|_| bounded) {
                    Some(delta) => round_roots(self, &rule.plan, delta),
                    None => rule.plan.roots(self),
                };
                stats.searched(roots.len(), delta.as_ref());
                if roots.is_empty() {
                    continue;
                }
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
