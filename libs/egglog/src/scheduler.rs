// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/scheduler.rs. See libs/egglog/LICENSE-MIT.

//! Schedulers choose which matches of a rule apply in an iteration; the
//! others wait for a later one. Each rule splits into a query that collects
//! its matches and an action rule that runs on the chosen ones.

use std::cell::Cell;
use std::collections::HashMap;

use tir_adt::FxBuildHasher;

use crate::ast::{Action, Change, Command, Expr, Fact, GenericActions, Rule, RuleEvalMode};
use crate::rules::typed;
use crate::sort::PROBE;
use crate::{
    EGraph, Error, IterationReport, ReportLevel, RuleReport, RunReport, Sink, TableId, Value, span,
};

/// What a scheduler may read of the e-graph.
pub struct SchedulerContext<'a> {
    pub egraph: &'a EGraph,
    num_nodes: &'a Cell<Option<usize>>,
}

impl SchedulerContext<'_> {
    /// The number of e-nodes, counted once until the e-graph changes.
    pub fn num_nodes(&self) -> usize {
        let count = self
            .num_nodes
            .get()
            .unwrap_or_else(|| self.egraph.num_nodes());
        self.num_nodes.set(Some(count));
        count
    }
}

/// A copy of a scheduler, for clones of the e-graph; implemented for every
/// `Clone` scheduler.
pub trait SchedulerClone {
    fn box_clone(&self) -> Box<dyn Scheduler>;
}

impl<T: Scheduler + Clone + 'static> SchedulerClone for T {
    fn box_clone(&self) -> Box<dyn Scheduler> {
        Box::new(self.clone())
    }
}

/// Decides which matches of a rule apply now.
pub trait Scheduler: SchedulerClone + Send + Sync {
    /// Whether the rules count as saturated once an iteration changed
    /// nothing.
    fn can_stop(&mut self, ctx: &SchedulerContext<'_>, rules: &[&str], ruleset: &str) -> bool {
        let _ = (ctx, rules, ruleset);
        true
    }

    /// Chooses matches of `rule` to apply. Returns whether the next
    /// iteration should look for new matches of it.
    fn filter_matches(
        &mut self,
        ctx: &SchedulerContext<'_>,
        rule: &str,
        ruleset: &str,
        matches: &mut Matches,
    ) -> bool;
}

/// The matches of a rule: the values of the variables its actions read.
pub struct Matches {
    values: Vec<Value>,
    rows: usize,
    vars: Vec<String>,
    chosen: Vec<usize>,
    all_chosen: bool,
}

/// One match: a value per variable.
pub struct Match<'a> {
    values: &'a [Value],
    vars: &'a [String],
}

impl Match<'_> {
    pub fn get_value(&self, var: &str) -> Value {
        let at = self
            .vars
            .iter()
            .position(|v| v == var)
            .expect("a variable of the rule");
        self.values[at]
    }
}

impl Matches {
    pub fn match_size(&self) -> usize {
        self.rows
    }

    pub fn tuple_len(&self) -> usize {
        self.vars.len()
    }

    pub fn get_match(&self, idx: usize) -> Match<'_> {
        let width = self.vars.len();
        Match {
            values: &self.values[idx * width..(idx + 1) * width],
            vars: &self.vars,
        }
    }

    /// Applies the match at `idx` in this iteration.
    pub fn choose(&mut self, idx: usize) {
        self.chosen.push(idx);
    }

    pub fn choose_all(&mut self) {
        self.all_chosen = true;
    }

    pub fn any_chosen(&self) -> bool {
        (self.all_chosen && self.rows > 0) || !self.chosen.is_empty()
    }

    // The chosen rows and the others.
    fn split(mut self) -> (Vec<Vec<Value>>, Vec<Value>, usize) {
        if self.all_chosen {
            self.chosen = (0..self.rows).collect();
        }
        self.chosen.sort_unstable();
        self.chosen.dedup();
        let width = self.vars.len();
        let row = |i: usize| self.values[i * width..(i + 1) * width].to_vec();
        let chosen = self.chosen.iter().map(|&i| row(i)).collect();
        let rest: Vec<usize> = (0..self.rows)
            .filter(|i| self.chosen.binary_search(i).is_err())
            .collect();
        (
            chosen,
            rest.iter().flat_map(|&i| row(i)).collect(),
            rest.len(),
        )
    }
}

/// A scheduler of an [`EGraph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SchedulerId(usize);

// A rule as a scheduler runs it.
#[derive(Clone)]
struct RuleInfo {
    sink: Sink,
    decided: TableId,
    query: String,
    apply: String,
    vars: Vec<String>,
    should_seek: bool,
    seminaive: bool,
    // Matches not chosen yet, and how many.
    residual: (Vec<Value>, usize),
}

pub(crate) struct SchedulerRecord {
    scheduler: Box<dyn Scheduler>,
    rules: HashMap<String, HashMap<String, RuleInfo, FxBuildHasher>, FxBuildHasher>,
}

impl Clone for SchedulerRecord {
    fn clone(&self) -> Self {
        Self {
            scheduler: self.scheduler.box_clone(),
            rules: self.rules.clone(),
        }
    }
}

fn vars(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Var(_, name) if !out.contains(name) => out.push(name.clone()),
        Expr::Call(_, _, args) => args.iter().for_each(|arg| vars(arg, out)),
        _ => {}
    }
}

// The variables of the query of `rule` that its actions read.
fn free_vars(rule: &Rule) -> Vec<String> {
    let mut query = Vec::new();
    for fact in &rule.body {
        match fact {
            Fact::Eq(_, a, b) => [a, b].into_iter().for_each(|e| vars(e, &mut query)),
            Fact::Fact(e) | Fact::Not(e) => vars(e, &mut query),
        }
    }
    let mut read = Vec::new();
    for action in &rule.head.0 {
        match action {
            Action::Let(_, _, e) | Action::Expr(_, e) => vars(e, &mut read),
            Action::Set(_, _, args, e) => args.iter().chain([e]).for_each(|e| vars(e, &mut read)),
            Action::Change(_, _, _, args) => args.iter().for_each(|e| vars(e, &mut read)),
            Action::Union(_, a, b) => [a, b].into_iter().for_each(|e| vars(e, &mut read)),
            Action::Panic(..) => {}
        }
    }
    read.retain(|var| query.contains(var));
    read
}

fn call(head: &str, vars: &[String]) -> Expr {
    let args = vars
        .iter()
        .map(|var| Expr::Var(span!(), var.clone()))
        .collect();
    Expr::Call(span!(), head.to_owned(), args)
}

impl EGraph {
    pub fn add_scheduler(&mut self, scheduler: Box<dyn Scheduler>) -> SchedulerId {
        self.schedulers.push(Some(SchedulerRecord {
            scheduler,
            rules: HashMap::default(),
        }));
        SchedulerId(self.schedulers.len() - 1)
    }

    pub fn remove_scheduler(&mut self, scheduler_id: SchedulerId) -> Option<Box<dyn Scheduler>> {
        let record = self.schedulers.get_mut(scheduler_id.0)?.take()?;
        Some(record.scheduler)
    }

    // The query and action rules of `rule`, and the relation of its chosen
    // matches.
    fn split_rule(&mut self, rule: &Rule) -> Result<RuleInfo, Error> {
        let vars = free_vars(rule);
        let probes = vars
            .iter()
            .map(|var| Action::Expr(span!(), call(PROBE, std::slice::from_ref(var))));
        let probe = Rule {
            head: GenericActions(probes.collect()),
            ..rule.clone()
        };
        let (_, probe) = typed(&self.db, probe)?;
        let sorts: Vec<_> = (probe.actions.iter())
            .filter_map(|act| match act {
                crate::plan::hir::Act::Call(prim, _) => {
                    Some(self.db.prims[prim.index()].signature.inputs[0])
                }
                _ => None,
            })
            .collect();
        let (query, apply, decided, collect) = (
            self.fresh("query"),
            self.fresh("apply"),
            self.fresh("decided"),
            self.fresh("collect"),
        );
        let sink = self.sink(&collect, &sorts);
        let mut chosen_sorts = vec![self.db.sort("i64")?];
        chosen_sorts.extend(&sorts);
        let decided_table = self.db.add_relation(&decided, &chosen_sorts);
        let head = GenericActions(vec![Action::Expr(span!(), call(&collect, &vars))]);
        let seeker = Rule {
            head,
            name: query.clone(),
            ruleset: query.clone(),
            ..rule.clone()
        };
        let mut actions = rule.head.0.clone();
        let mut occurrence = self.fresh("match");
        while vars.contains(&occurrence)
            || rule
                .head
                .0
                .iter()
                .any(|action| matches!(action, Action::Let(_, name, _) if *name == occurrence))
        {
            occurrence = self.fresh("match");
        }
        let chosen_vars: Vec<_> = std::iter::once(occurrence)
            .chain(vars.iter().cloned())
            .collect();
        let chosen = call(&decided, &chosen_vars);
        let Expr::Call(_, _, args) = &chosen else {
            unreachable!("a call")
        };
        actions.push(Action::Change(
            span!(),
            Change::Delete,
            decided.clone(),
            args.clone(),
        ));
        let body = vec![Fact::Fact(chosen)];
        let applier = Rule {
            head: GenericActions(actions),
            body,
            name: apply.clone(),
            ruleset: apply.clone(),
            eval_mode: RuleEvalMode::Naive,
            ..rule.clone()
        };
        for rule in [seeker, applier] {
            self.rules.add_ruleset(&rule.ruleset);
            self.rules.add_command(&self.db, Command::Rule { rule })?;
        }
        let seminaive = self.seminaive && rule.eval_mode == RuleEvalMode::Seminaive;
        Ok(RuleInfo {
            sink,
            decided: decided_table,
            query,
            apply,
            vars,
            should_seek: true,
            seminaive,
            residual: (Vec::new(), 0),
        })
    }

    /// One iteration of a ruleset whose matches `scheduler_id` chooses. A
    /// failed action keeps what ran before it and drops the chosen matches.
    pub fn step_rules_with_scheduler(
        &mut self,
        scheduler_id: SchedulerId,
        ruleset: &str,
    ) -> Result<RunReport, Error> {
        if !self.has_ruleset(ruleset) {
            return Err(Error::NoSuchRuleset(ruleset.to_owned(), span!()));
        }
        let missing = || Error::UnknownName(format!("scheduler {}", scheduler_id.0));
        let mut record = self
            .schedulers
            .get_mut(scheduler_id.0)
            .and_then(Option::take)
            .ok_or_else(missing)?;
        let result = self.step_with(&mut record, ruleset);
        self.schedulers[scheduler_id.0] = Some(record);
        result
    }

    fn step_with(
        &mut self,
        record: &mut SchedulerRecord,
        ruleset: &str,
    ) -> Result<RunReport, Error> {
        let rules: Vec<Rule> = self.rules.rules_of(ruleset).into_iter().cloned().collect();
        for rule in &rules {
            if !record
                .rules
                .get(&rule.ruleset)
                .is_some_and(|rules| rules.contains_key(&rule.name))
            {
                let info = self.split_rule(rule)?;
                record
                    .rules
                    .entry(rule.ruleset.clone())
                    .or_default()
                    .insert(rule.name.clone(), info);
            }
        }
        self.plan()?;
        let mut queries = Vec::new();
        for rule in &rules {
            let info = record
                .rules
                .get_mut(&rule.ruleset)
                .and_then(|rules| rules.get_mut(&rule.name))
                .expect("split above");
            if !info.should_seek {
                continue;
            }
            if !info.seminaive {
                info.residual = (Vec::new(), 0);
            }
            *info.sink.lock().expect("a sink lock") = (0, Vec::new());
            queries.push(info.query.clone());
        }
        let queries: Vec<&str> = queries.iter().map(String::as_str).collect();
        self.program.step(&mut self.db, &queries, true)?;
        for rule in &rules {
            let info = record
                .rules
                .get_mut(&rule.ruleset)
                .and_then(|rules| rules.get_mut(&rule.name))
                .expect("split above");
            let (rows, values) = std::mem::take(&mut *info.sink.lock().expect("a sink lock"));
            info.residual.0.extend(values);
            info.residual.1 += rows;
        }
        let before = self.db.changes();
        let mut bookkeeping = 0;
        let (num_nodes, mut iteration) = (Cell::new(None), IterationReport::default());
        let mut applied_any = false;
        for rule in &rules {
            let info = record
                .rules
                .get_mut(&rule.ruleset)
                .and_then(|rules| rules.get_mut(&rule.name))
                .expect("split above");
            let (values, rows) = std::mem::take(&mut info.residual);
            let mut matches = Matches {
                values,
                rows,
                vars: info.vars.clone(),
                chosen: Vec::new(),
                all_chosen: false,
            };
            let ctx = SchedulerContext {
                egraph: self,
                num_nodes: &num_nodes,
            };
            info.should_seek =
                record
                    .scheduler
                    .filter_matches(&ctx, &rule.name, ruleset, &mut matches);
            let (chosen, rest, left) = matches.split();
            info.residual = (rest, left);
            if chosen.is_empty() {
                continue;
            }
            let version = self.db.tables[info.decided.index()].version();
            for (at, row) in chosen.iter().enumerate() {
                let mut selected = Vec::with_capacity(row.len() + 1);
                selected.push(self.db.base(at as i64));
                selected.extend(row);
                self.db.insert(info.decided, &selected)?;
            }
            // As upstream, each rule's actions apply at once and one rebuild
            // follows the last of them.
            let applied = self.program.step(&mut self.db, &[&info.apply], false);
            num_nodes.set(None);
            let applied = match applied {
                Ok(applied) => applied,
                Err(error) => {
                    self.db.clear(info.decided);
                    self.db.rebuild()?;
                    return Err(error);
                }
            };
            bookkeeping += self.db.tables[info.decided.index()].version() - version;
            if self.report_level != ReportLevel::TimeOnly {
                let report = RuleReport {
                    search_and_apply_time: applied.search_and_apply_time(),
                    num_matches: chosen.len(),
                };
                let reports = &mut iteration.rule_set_report.rule_reports;
                reports
                    .entry(rule.name.as_str().into())
                    .or_default()
                    .push(report);
            }
            iteration.rule_set_report.search_and_apply_time += applied.search_and_apply_time();
            iteration.rule_set_report.merge_time += applied.rule_set_report.merge_time;
            applied_any = true;
        }
        if applied_any {
            let start = std::time::Instant::now();
            self.db.rebuild()?;
            iteration.rebuild_time = start.elapsed();
        }
        iteration.rule_set_report.changed = self.db.changes() != before + bookkeeping;
        let mut report = RunReport::of(ruleset, iteration, self.report_level);
        let names: Vec<&str> = rules.iter().map(|rule| rule.name.as_str()).collect();
        let ctx = SchedulerContext {
            egraph: self,
            num_nodes: &num_nodes,
        };
        report.can_stop = !report.updated && record.scheduler.can_stop(&ctx, &names, ruleset);
        Ok(report)
    }
}
