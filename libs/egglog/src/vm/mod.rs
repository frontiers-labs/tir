//! Compiled rulesets and the interpreter that runs them.

pub(crate) mod code;
mod exec;
mod file;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

pub use file::LoadError;
pub(crate) use file::{
    Const, ConstDecl, ExternDecl, IndexDecl, ProgramFile, RulesetDecl, SortClass, SortDecl,
    TableDecl, decode, map_merge,
};

use crate::ast::{Fact, Literal, Schedule};
use crate::report::{IterationReport, ReportLevel, RuleReport, RuleSetReport};
use crate::{BaseValues, Color, Database, Error, ExternId, IndexId, RunReport, TableId, Value};
use code::Step;

/// The rule tries of a set of rulesets, planned from the schema alone.
///
/// A program runs against the database it was compiled or loaded for. It
/// remembers when each rule last ran, so a later run only matches what
/// involves rows written since.
#[derive(Clone)]
pub struct Program {
    // Shared by the clones of a program; only its run state is their own.
    file: Arc<ProgramFile>,
    // The database ids of the tables and primitives of the file.
    tables: Vec<TableId>,
    externs: Vec<ExternId>,
    // Set by the first run: indexes, constants and the indexes each ruleset
    // reads.
    prepared: bool,
    indexes: Vec<IndexId>,
    consts: Vec<Vec<Value>>,
    used: Vec<Vec<IndexId>>,
    // The rule names of each ruleset, for reports above TimeOnly; made on
    // the first such report.
    names: Vec<Vec<Arc<str>>>,
    // Per ruleset of the file. The rules of a ruleset that ran at different
    // times are planned as separate rulesets of the same name.
    last_run: Vec<Option<u32>>,
    // Per ruleset, by color: the color timestamp it last ran at, 0 if never.
    color_last: Vec<Vec<u32>>,
    // Whether every run matches the whole database.
    naive: bool,
    level: ReportLevel,
}

type Reports = HashMap<Arc<str>, Vec<RuleReport>, tir_adt::FxBuildHasher>;

// The name of the query ruleset that `:until` facts are planned into.
pub(crate) fn until_name(facts: &[Fact]) -> String {
    let facts: Vec<String> = facts.iter().map(ToString::to_string).collect();
    format!("@until {}", facts.join(" "))
}

impl Program {
    pub(crate) fn new(
        file: ProgramFile,
        tables: Vec<TableId>,
        externs: Vec<ExternId>,
        last_run: Vec<Option<u32>>,
    ) -> Self {
        Self {
            file: Arc::new(file),
            tables,
            externs,
            prepared: false,
            indexes: Vec::new(),
            consts: Vec::new(),
            used: Vec::new(),
            names: Vec::new(),
            last_run,
            color_last: Vec::new(),
            naive: false,
            level: ReportLevel::TimeOnly,
        }
    }

    /// What runs report: the matches of each rule and each iteration only
    /// above [`ReportLevel::TimeOnly`].
    pub fn set_report_level(&mut self, level: ReportLevel) {
        self.level = level;
    }

    /// Whether every run of a rule matches the whole database rather than
    /// what is new since its last run.
    pub fn set_naive(&mut self, naive: bool) {
        self.naive = naive;
    }

    /// Loads a program that [`Program::to_bytes`] wrote. Sorts, tables and
    /// primitives bind to `db` by name; the Eq sorts and tables `db` lacks
    /// are declared.
    pub fn from_bytes(bytes: &[u8], db: &mut Database) -> Result<Self, LoadError> {
        let file = file::decode(bytes)?;
        let (tables, externs) = file::bind(&file, db)?;
        let last_run = vec![None; file.rulesets.len()];
        Ok(Self::new(file, tables, externs, last_run))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        file::encode(&self.file)
    }

    /// Runs the schedule. Each iteration of a ruleset matches every rule,
    /// then applies the writes of all matches and rebuilds. A `:until` needs
    /// its query planned, see [`Rules::plan_schedule`](crate::Rules::plan_schedule).
    pub fn run(&mut self, db: &mut Database, schedule: &Schedule) -> Result<RunReport, Error> {
        self.prepare(db)?;
        db.rebuild()?;
        self.schedule(db, schedule)
    }

    // The names of the rulesets, combined ones and queries included.
    pub(crate) fn rulesets(&self) -> impl Iterator<Item = &str> {
        self.file
            .rulesets
            .iter()
            .map(|ruleset| ruleset.name.as_str())
    }

    // When `rule` of `ruleset` last ran, if it is in this program and ran.
    pub(crate) fn last_run_of(&self, ruleset: &str, rule: &str) -> Option<u32> {
        let decls = self.file.rulesets.iter().zip(&self.last_run);
        let mut found = decls.filter(|(decl, _)| decl.name == ruleset);
        found
            .find(|(decl, _)| decl.rules.iter().any(|name| name == rule))
            .and_then(|(_, last)| *last)
    }

    fn prepare(&mut self, db: &mut Database) -> Result<(), Error> {
        if self.prepared {
            return Ok(());
        }
        for (decl, &table) in self.file.tables.iter().zip(&self.tables) {
            for order in &decl.symmetries {
                db.add_symmetry(table, order)?;
            }
        }
        let tables = &self.tables;
        self.indexes = (self.file.indexes.iter())
            .map(|index| db.add_index(tables[index.table as usize], &index.order))
            .collect();
        self.consts = (self.file.rulesets.iter())
            .map(|ruleset| ruleset.consts.iter().map(|c| value(&db.base, c)).collect())
            .collect();
        self.used = (self.file.rulesets.iter())
            .map(|ruleset| {
                let mut used = Vec::new();
                let mut stack = ruleset.roots.clone();
                while let Some(node) = stack.pop() {
                    let node = &self.file.nodes[node as usize];
                    if let Step::Expand { index, .. } | Step::Check { index, .. } = node.step {
                        used.push(self.indexes[index as usize]);
                    }
                    stack.extend(&node.children);
                }
                used.sort_unstable();
                used.dedup();
                used
            })
            .collect();

        self.prepared = true;
        Ok(())
    }

    fn schedule(&mut self, db: &mut Database, schedule: &Schedule) -> Result<RunReport, Error> {
        let mut report = RunReport::default();
        match schedule {
            Schedule::Run(_, config) => {
                if let Some(facts) = &config.until
                    && self.matches(db, &until_name(facts))?
                {
                    return Ok(report);
                }
                let iteration = self.step(db, &[&config.ruleset], true)?;
                return Ok(RunReport::of(&config.ruleset, iteration, self.level));
            }
            Schedule::Repeat(_, times, schedule) => {
                for _ in 0..*times {
                    let run = self.schedule(db, schedule)?;
                    let stop = run.can_stop;
                    report.union(run);
                    if stop {
                        break;
                    }
                }
            }
            Schedule::Saturate(_, schedule) => loop {
                let run = self.schedule(db, schedule)?;
                let updated = run.updated;
                report.union(run);
                if !updated {
                    break;
                }
            },
            Schedule::Sequence(_, schedules) => {
                for schedule in schedules {
                    report.union(self.schedule(db, schedule)?);
                }
            }
        }
        Ok(report)
    }

    // The rulesets of the file that a run of `name` runs: those of this name,
    // and the members of a combined one, recursively.
    fn decls(&self, name: &str, out: &mut Vec<usize>) -> Result<(), Error> {
        let mut found = false;
        for (at, decl) in self.file.rulesets.iter().enumerate() {
            if decl.name != name {
                continue;
            }
            found = true;
            if decl.members.is_empty() && !out.contains(&at) {
                out.push(at);
            }
            for member in &decl.members {
                self.decls(member, out)?;
            }
        }
        found
            .then_some(())
            .ok_or_else(|| Error::UnknownName(name.to_owned()))
    }

    // Whether the query ruleset `name` matches anything.
    pub(crate) fn matches(&mut self, db: &mut Database, name: &str) -> Result<bool, Error> {
        self.prepare(db)?;
        let mut decls = Vec::new();
        self.decls(name, &mut decls)?;
        for decl in decls {
            let (_, matches, result) = exec::search(self, db, decl, 0);
            result?;
            if matches.iter().any(|&n| n > 0) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // One iteration of the ruleset `name`: every rule matches with rows
    // since its own last run as New, then the writes of all matches apply.
    // A search that fails still applies what it wrote before the failure.
    // One iteration of the rulesets `names`. Without `rebuild` the writes stay
    // uncanonical, for a caller that rebuilds once after several steps.
    pub(crate) fn step(
        &mut self,
        db: &mut Database,
        names: &[&str],
        rebuild: bool,
    ) -> Result<IterationReport, Error> {
        self.prepare(db)?;
        let mut decls = Vec::new();
        for name in names {
            self.decls(name, &mut decls)?;
        }
        let (now, before) = (db.timestamp(), db.changes());
        let colored = (rebuild && !db.colors.is_empty()).then(|| db.colors.changes());
        let start = Instant::now();
        let mut rule_reports = Reports::default();
        let mut writes = Vec::new();
        let mut failed = Ok(());
        let once = |at: usize| self.file.rulesets[at].once && self.last_run[at].is_some();
        decls.retain(|&decl| self.naive || !once(decl));
        for &decl in &decls {
            let last = self.last_run[decl].filter(|_| !self.naive).unwrap_or(0);
            let (found, matches, result) = exec::search(self, db, decl, last);
            writes.push(found);
            self.report(decl, matches, &mut rule_reports);
            failed = result;
            if failed.is_err() {
                break;
            }
        }
        let searched = Instant::now();
        exec::Writes::apply(writes, db, &self.tables)?;
        let applied = Instant::now();
        if rebuild {
            db.rebuild()?;
        }
        failed?;
        if colored.is_some() {
            self.step_colors(db, &decls, &mut rule_reports)?;
        }
        for decl in decls {
            self.last_run[decl] = Some(now);
        }
        let colors_changed = colored.is_some_and(|before| db.colors.changes() != before);
        Ok(IterationReport {
            rule_set_report: RuleSetReport {
                changed: db.changes() != before || colors_changed,
                rule_reports,
                search_and_apply_time: searched - start,
                merge_time: applied - searched,
            },
            rebuild_time: applied.elapsed(),
        })
    }

    // One iteration of `decls` in the colors, after the base: each level, the
    // lowest first, imports what its parents wrote, matches every color of
    // the level in one search per ruleset, then applies and rebuilds.
    fn step_colors(
        &mut self,
        db: &mut Database,
        decls: &[usize],
        rule_reports: &mut Reports,
    ) -> Result<(), Error> {
        self.color_last.resize(self.file.rulesets.len(), Vec::new());
        for level in db.colors.levels() {
            db.import_colors(&level)?;
            let now = db.colors.timestamp();
            let mut writes = Vec::new();
            let mut failed = Ok(());
            for &decl in decls {
                let last: Vec<u32> = (level.iter())
                    .map(|&c| self.color_last(db, decl, c))
                    .collect();
                let (found, matches, result) = exec::search_colors(self, db, decl, &level, &last);
                writes.push(found);
                self.report(decl, matches, rule_reports);
                failed = result;
                if failed.is_err() {
                    break;
                }
            }
            exec::Writes::apply_colors(writes, db, &self.tables)?;
            db.rebuild_colors()?;
            failed?;
            for &decl in decls {
                let last = &mut self.color_last[decl];
                last.resize(db.colors.len(), 0);
                level.iter().for_each(|c| last[c.index()] = now);
            }
        }
        Ok(())
    }

    // The color timestamp from which the rows of `color` are New to `decl`:
    // all of them once the color read new base rows since `decl` last ran.
    fn color_last(&self, db: &Database, decl: usize, color: Color) -> u32 {
        let last = self.color_last[decl].get(color.index()).copied();
        let last = last.filter(|_| !self.naive).unwrap_or(0);
        if db.colors.rebased(color) >= last {
            0
        } else {
            last
        }
    }

    // Adds the matches of a search of `decl` to the reports above TimeOnly.
    fn report(&mut self, decl: usize, matches: Vec<usize>, rule_reports: &mut Reports) {
        if self.level == ReportLevel::TimeOnly {
            return;
        }
        if self.names.is_empty() {
            self.names = (self.file.rulesets.iter())
                .map(|ruleset| {
                    ruleset
                        .rules
                        .iter()
                        .map(|rule| Arc::from(rule.as_str()))
                        .collect()
                })
                .collect();
        }
        let matched = self.names[decl].iter().zip(matches).filter(|(_, n)| *n > 0);
        for (rule, num_matches) in matched {
            let report = RuleReport {
                num_matches,
                ..RuleReport::default()
            };
            rule_reports.entry(rule.clone()).or_default().push(report);
        }
    }
}

// The cell of a literal.
pub(crate) fn value(base: &BaseValues, literal: &Literal) -> Value {
    match literal {
        Literal::Int(i) => base.get(*i),
        Literal::Float(x) => base.get(*x),
        Literal::String(s) => base.get(s.clone()),
        Literal::Bool(b) => base.get(*b),
        Literal::Unit => base.get(()),
    }
}
