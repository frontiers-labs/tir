// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, egglog-reports/src/lib.rs. See libs/egglog/LICENSE-MIT.

use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::sync::Arc;
use std::time::Duration;

use tir_adt::FxBuildHasher;

type Map<V> = HashMap<Arc<str>, V, FxBuildHasher>;

/// How much a run reports. Every level reports the time of each phase per
/// ruleset; the levels above `TimeOnly` also report the matches of each rule
/// and each iteration. Query plans are not reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReportLevel {
    /// The time taken in each search, merge and rebuild phase.
    #[default]
    TimeOnly,
    WithPlan,
    StageInfo,
}

/// What one rule that matched did in one iteration. Rules of a ruleset share
/// their search, so a rule reports no time of its own.
#[derive(Clone, Debug, Default)]
pub struct RuleReport {
    pub search_and_apply_time: Duration,
    pub num_matches: usize,
}

/// What the rules of one iteration did.
#[derive(Clone, Debug, Default)]
pub struct RuleSetReport {
    pub changed: bool,
    pub rule_reports: Map<Vec<RuleReport>>,
    pub search_and_apply_time: Duration,
    pub merge_time: Duration,
}

impl RuleSetReport {
    pub fn num_matches(&self, rule: &str) -> usize {
        self.rule_reports
            .get(rule)
            .map_or(0, |reports| reports.iter().map(|r| r.num_matches).sum())
    }

    pub fn rule_search_and_apply_time(&self, rule: &str) -> Duration {
        self.rule_reports
            .get(rule)
            .map_or(Duration::ZERO, |reports| {
                reports.iter().map(|r| r.search_and_apply_time).sum()
            })
    }
}

/// One iteration of a ruleset.
#[derive(Clone, Debug, Default)]
pub struct IterationReport {
    pub rule_set_report: RuleSetReport,
    pub rebuild_time: Duration,
}

impl IterationReport {
    pub fn changed(&self) -> bool {
        self.rule_set_report.changed
    }

    pub fn search_and_apply_time(&self) -> Duration {
        self.rule_set_report.search_and_apply_time
    }

    pub fn rule_reports(&self) -> &Map<Vec<RuleReport>> {
        &self.rule_set_report.rule_reports
    }

    pub fn rules(&self) -> impl Iterator<Item = &Arc<str>> {
        self.rule_set_report.rule_reports.keys()
    }
}

/// What a run of a schedule did: its iterations, whether the database
/// changed, and times and matches per rule and ruleset. [`RunReport::union`]
/// adds two reports together.
#[derive(Clone, Debug)]
pub struct RunReport {
    pub iterations: Vec<Arc<IterationReport>>,
    /// Whether an iteration added a row, changed an output or united two
    /// e-classes.
    pub updated: bool,
    /// Whether the run changed nothing and no scheduler holds back matches
    /// for another iteration.
    pub can_stop: bool,
    pub search_and_apply_time_per_rule: Map<Duration>,
    pub num_matches_per_rule: Map<usize>,
    pub search_and_apply_time_per_ruleset: Map<Duration>,
    pub merge_time_per_ruleset: Map<Duration>,
    pub rebuild_time_per_ruleset: Map<Duration>,
}

impl Default for RunReport {
    fn default() -> Self {
        Self {
            iterations: Vec::new(),
            updated: false,
            can_stop: true,
            search_and_apply_time_per_rule: Map::default(),
            num_matches_per_rule: Map::default(),
            search_and_apply_time_per_ruleset: Map::default(),
            merge_time_per_ruleset: Map::default(),
            rebuild_time_per_ruleset: Map::default(),
        }
    }
}

impl Display for RunReport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut rules: Vec<_> = self.search_and_apply_time_per_rule.iter().collect();
        rules.sort_by_key(|(_, time)| **time);
        for (rule, time) in rules {
            let mut name = rule.replace('\n', " ");
            if name.len() > 80 {
                name.truncate(name.floor_char_boundary(80));
                name.push_str("...");
            }
            let matches = self.num_matches_per_rule.get(rule).copied().unwrap_or(0);
            let time = time.as_secs_f64();
            writeln!(
                f,
                "Rule {name}: search and apply {time:.3}s, num matches {matches}"
            )?;
        }
        let mut rulesets: Vec<&Arc<str>> = (self.search_and_apply_time_per_ruleset.keys())
            .chain(self.merge_time_per_ruleset.keys())
            .chain(self.rebuild_time_per_ruleset.keys())
            .collect();
        rulesets.sort();
        rulesets.dedup();
        let time =
            |map: &Map<Duration>, ruleset| map.get(ruleset).map_or(0.0, Duration::as_secs_f64);
        for ruleset in rulesets {
            let search = time(&self.search_and_apply_time_per_ruleset, ruleset);
            let merge = time(&self.merge_time_per_ruleset, ruleset);
            let rebuild = time(&self.rebuild_time_per_ruleset, ruleset);
            writeln!(
                f,
                "Ruleset {ruleset}: search {search:.3}s, merge {merge:.3}s, rebuild {rebuild:.3}s"
            )?;
        }
        Ok(())
    }
}

fn add<V: std::ops::AddAssign + Default>(into: &mut Map<V>, from: Map<V>) {
    if into.is_empty() {
        *into = from;
        return;
    }
    for (key, value) in from {
        *into.entry(key).or_default() += value;
    }
}

impl RunReport {
    /// The report of one iteration of `ruleset`.
    pub fn singleton(ruleset: &str, iteration: IterationReport) -> Self {
        let mut report = Self::default();
        let rules = &iteration.rule_set_report;
        let count = rules.rule_reports.len();
        report.search_and_apply_time_per_rule =
            Map::with_capacity_and_hasher(count, FxBuildHasher::default());
        report.num_matches_per_rule =
            Map::with_capacity_and_hasher(count, FxBuildHasher::default());
        for rule in iteration.rules() {
            let time = rules.rule_search_and_apply_time(rule);
            *report
                .search_and_apply_time_per_rule
                .entry(rule.clone())
                .or_default() += time;
            *report.num_matches_per_rule.entry(rule.clone()).or_default() +=
                rules.num_matches(rule);
        }
        let ruleset: Arc<str> = ruleset.into();
        let per_ruleset = |time| Map::from_iter([(ruleset.clone(), time)]);
        report.search_and_apply_time_per_ruleset = per_ruleset(iteration.search_and_apply_time());
        report.merge_time_per_ruleset = per_ruleset(rules.merge_time);
        report.rebuild_time_per_ruleset = per_ruleset(iteration.rebuild_time);
        report.updated = iteration.changed();
        report.can_stop = !report.updated;
        report.iterations.push(Arc::new(iteration));
        report
    }

    // The report of one iteration at `level`: below the per-rule levels, only
    // the times of the ruleset and whether it changed anything.
    pub(crate) fn of(ruleset: &str, iteration: IterationReport, level: ReportLevel) -> Self {
        if level != ReportLevel::TimeOnly {
            return Self::singleton(ruleset, iteration);
        }
        let ruleset: Arc<str> = ruleset.into();
        let per_ruleset = |time| Map::from_iter([(ruleset.clone(), time)]);
        let updated = iteration.changed();
        Self {
            search_and_apply_time_per_ruleset: per_ruleset(iteration.search_and_apply_time()),
            merge_time_per_ruleset: per_ruleset(iteration.rule_set_report.merge_time),
            rebuild_time_per_ruleset: per_ruleset(iteration.rebuild_time),
            updated,
            can_stop: !updated,
            ..Self::default()
        }
    }

    pub fn add_iteration(&mut self, ruleset: &str, iteration: IterationReport) {
        self.union(Self::singleton(ruleset, iteration));
    }

    /// Adds `other` to this report.
    pub fn union(&mut self, other: Self) {
        self.iterations.extend(other.iterations);
        self.updated |= other.updated;
        self.can_stop &= other.can_stop;
        add(
            &mut self.search_and_apply_time_per_rule,
            other.search_and_apply_time_per_rule,
        );
        add(&mut self.num_matches_per_rule, other.num_matches_per_rule);
        add(
            &mut self.search_and_apply_time_per_ruleset,
            other.search_and_apply_time_per_ruleset,
        );
        add(
            &mut self.merge_time_per_ruleset,
            other.merge_time_per_ruleset,
        );
        add(
            &mut self.rebuild_time_per_ruleset,
            other.rebuild_time_per_ruleset,
        );
    }
}
