//! Typed rules grouped in rulesets, and their compilation into a [`Program`].

use crate::ast::{
    Action, Actions, Command, Expr, Fact, Facts, Literal, Rewrite, Rule, RuleEvalMode, RunConfig,
    Schedule,
};
use crate::desugar::{desugar, remove_globals};
use crate::plan::hir::Target;
use crate::plan::{self, Source, hir};
use crate::sort::FN_PREFIX;
use crate::typecheck::typecheck;
use crate::vm::until_name;
use crate::{Database, Error, Program, TableKind, span};

/// How a ruleset may be planned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// The tables hold after each iteration what egglog would hold.
    #[default]
    Strict,
    /// A rule that only permutes the columns of one table, such as
    /// commutativity, becomes a symmetry of the table: every row is written
    /// in each of its orders at once. The tables hold the same rows at a
    /// fixed point, sooner.
    Relaxed,
}

#[derive(Clone)]
struct Ruleset {
    name: String,
    mode: Mode,
    // A query keeps its rules with no actions, to count their matches.
    query: bool,
    // The rulesets a combined ruleset runs together; it has no rules.
    members: Vec<String>,
    // Whether the rules match in the colors only.
    colored: bool,
    // Each rule with globals made explicit, and typed.
    rules: Vec<(Rule, hir::Rule)>,
}

/// Rules checked against the schema of a [`Database`], by ruleset.
#[derive(Clone, Default)]
pub struct Rules {
    rulesets: Vec<Ruleset>,
}

impl Rules {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares an empty ruleset; adding a rule declares its ruleset too.
    pub fn add_ruleset(&mut self, name: &str) {
        self.ruleset(name);
    }

    /// Declares a ruleset whose run is one iteration of the rules of all
    /// `members`, each of which must be declared.
    pub fn add_combined_ruleset(&mut self, name: &str, members: &[String]) -> Result<(), Error> {
        if let Some(missing) = members.iter().find(|member| !self.has_ruleset(member)) {
            return Err(Error::NoSuchRuleset(missing.clone(), span!()));
        }
        self.ruleset(name).members = members.to_vec();
        Ok(())
    }

    pub fn set_mode(&mut self, ruleset: &str, mode: Mode) {
        self.ruleset(ruleset).mode = mode;
    }

    /// Makes the rules of `ruleset` match in the colors only: what they
    /// write goes with the color, and each new color runs them afresh, as
    /// the seed of what a run derives there.
    pub fn set_colored(&mut self, ruleset: &str) {
        self.ruleset(ruleset).colored = true;
    }

    /// Adds the rule that runs `actions` for each match of `facts`.
    ///
    /// Table lookups in facts and actions read committed rows, not the writes
    /// queued earlier in the iteration. A missing function lookup fails even
    /// after a queued set; writes before the failure still apply. Constructor
    /// and default entries share their pending output when the key is missing.
    pub fn rule(
        &mut self,
        db: &Database,
        ruleset: &str,
        facts: Facts,
        actions: Actions,
    ) -> Result<(), Error> {
        let rule = Rule {
            span: span!(),
            head: actions,
            body: facts.0,
            name: String::new(),
            ruleset: ruleset.to_owned(),
            eval_mode: RuleEvalMode::Seminaive,
        };
        self.add_command(db, Command::Rule { rule })
    }

    /// Adds the rule that unions each match of `lhs` with `rhs`.
    pub fn rewrite(
        &mut self,
        db: &Database,
        ruleset: &str,
        lhs: Expr,
        rhs: Expr,
    ) -> Result<(), Error> {
        let rewrite = rewrite(lhs, rhs);
        self.add_command(db, Command::Rewrite(ruleset.to_owned(), rewrite, false))
    }

    /// Adds a rewrite in both directions.
    pub fn birewrite(
        &mut self,
        db: &Database,
        ruleset: &str,
        lhs: Expr,
        rhs: Expr,
    ) -> Result<(), Error> {
        let rewrite = rewrite(lhs, rhs);
        self.add_command(db, Command::BiRewrite(ruleset.to_owned(), rewrite))
    }

    /// Adds the rules of a `rule`, `rewrite` or `birewrite` command.
    pub fn add_command(&mut self, db: &Database, command: Command) -> Result<(), Error> {
        for command in desugar(command)? {
            let Command::Rule { rule } = command else {
                return Err(Error::Unsupported(format!("`{command}` is not a rule")));
            };
            if self.is_combined(&rule.ruleset) {
                return Err(Error::CombinedRulesetError(rule.ruleset, rule.span));
            }
            let (rule, typed) = typed(db, rule)?;
            self.ruleset(&rule.ruleset).rules.push((rule, typed));
        }
        Ok(())
    }

    /// Plans the queries of the `:until` facts in `schedule`, so a compiled
    /// program can run it.
    pub fn plan_schedule(&mut self, db: &Database, schedule: &Schedule) -> Result<(), Error> {
        match schedule {
            Schedule::Run(
                _,
                RunConfig {
                    until: Some(facts), ..
                },
            ) => self.add_query(db, &until_name(facts), facts.clone()),
            Schedule::Run(..) => Ok(()),
            Schedule::Repeat(_, _, schedule) | Schedule::Saturate(_, schedule) => {
                self.plan_schedule(db, schedule)
            }
            Schedule::Sequence(_, schedules) => {
                (schedules.iter()).try_for_each(|schedule| self.plan_schedule(db, schedule))
            }
        }
    }

    // A ruleset of one rule that matches `facts` and does nothing, for
    // `Program::matches`.
    pub(crate) fn add_query(
        &mut self,
        db: &Database,
        name: &str,
        facts: Vec<Fact>,
    ) -> Result<(), Error> {
        if self.has_ruleset(name) {
            return Ok(());
        }
        let rule = Rule {
            span: span!(),
            head: Actions::default(),
            body: facts,
            name: name.to_owned(),
            ruleset: name.to_owned(),
            eval_mode: RuleEvalMode::Naive,
        };
        let rule = typed(db, rule)?;
        let ruleset = self.ruleset(name);
        ruleset.query = true;
        ruleset.rules.push(rule);
        Ok(())
    }

    /// Plans every ruleset from the schema of `db`.
    pub fn compile(&self, db: &Database) -> Result<Program, Error> {
        self.compile_with(db, |_, _| None)
    }

    /// Plans every ruleset like [`Rules::compile`], keeping what `previous`
    /// remembers of when each rule last ran: those rules go on matching only
    /// what is new since, and the others match everything once.
    pub fn recompile(&self, db: &Database, previous: &Program) -> Result<Program, Error> {
        self.compile_with(db, |ruleset, rule| previous.last_run_of(ruleset, rule))
    }

    // The rules of a ruleset that last ran at different times are planned
    // as separate rulesets of the same name, each with its own time.
    fn compile_with(
        &self,
        db: &Database,
        last: impl Fn(&str, &str) -> Option<u32>,
    ) -> Result<Program, Error> {
        let (mut sources, mut last_run) = (Vec::new(), Vec::new());
        for ruleset in &self.rulesets {
            let mut groups: Vec<(Group, Vec<&hir::Rule>)> = Vec::new();
            for (_, rule) in &ruleset.rules {
                let args = (rule.constants.iter()).any(|c| matches!(c, Some(Literal::Arg(_))));
                let reads =
                    (rule.premise.iter()).any(|atom| matches!(atom.target, Target::Table(_)));
                let once = !rule.naive && !reads && !args && !ruleset.colored;
                let key = (last(&ruleset.name, &rule.name), once, args);
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, rules)) => rules.push(rule),
                    None => groups.push((key, vec![rule])),
                }
            }
            if groups.is_empty() {
                groups.push(((None, false, false), Vec::new()));
            }
            for ((time, once, _), rules) in groups {
                sources.push(Source {
                    name: &ruleset.name,
                    relaxed: ruleset.mode == Mode::Relaxed,
                    query: ruleset.query,
                    once,
                    colored: ruleset.colored,
                    rules,
                    members: &ruleset.members,
                });
                last_run.push(time);
            }
        }
        let (file, tables, externs) = plan::compile(&sources, db.schema())?;
        Ok(Program::new(file, tables, externs, last_run))
    }

    // Grows with every ruleset and rule added.
    pub(crate) fn count(&self) -> usize {
        let rules: usize = self
            .rulesets
            .iter()
            .map(|ruleset| ruleset.rules.len())
            .sum();
        self.rulesets.len() + rules
    }

    pub(crate) fn has_ruleset(&self, name: &str) -> bool {
        self.rulesets.iter().any(|ruleset| ruleset.name == name)
    }

    pub(crate) fn is_combined(&self, name: &str) -> bool {
        (self.rulesets.iter()).any(|ruleset| ruleset.name == name && !ruleset.members.is_empty())
    }

    // The rules a run of `name` runs, combined members included.
    pub(crate) fn rules_of(&self, name: &str) -> Vec<&Rule> {
        let Some(ruleset) = self.rulesets.iter().find(|ruleset| ruleset.name == name) else {
            return Vec::new();
        };
        let own = ruleset.rules.iter().map(|(rule, _)| rule);
        let members = ruleset
            .members
            .iter()
            .flat_map(|member| self.rules_of(member));
        own.chain(members).collect()
    }

    fn ruleset(&mut self, name: &str) -> &mut Ruleset {
        let at = (self
            .rulesets
            .iter()
            .position(|ruleset| ruleset.name == name))
        .unwrap_or_else(|| {
            self.rulesets.push(Ruleset {
                name: name.to_owned(),
                mode: Mode::Strict,
                query: false,
                members: Vec::new(),
                colored: false,
                rules: Vec::new(),
            });
            self.rulesets.len() - 1
        });
        &mut self.rulesets[at]
    }
}

// When the rules of a part of a ruleset last ran, whether they read no table
// and no run argument, and whether they read a run argument: those match
// everything again when it changes, apart from the rest.
type Group = (Option<u32>, bool, bool);

// The rule with its globals made explicit, and its typed form.
pub(crate) fn typed(db: &Database, rule: Rule) -> Result<(Rule, hir::Rule), Error> {
    let schema = db.schema();
    // A global is a nullary function, referred to by its name.
    let global = |name: &str| {
        schema.table_named(name).is_some_and(|table| {
            let table = schema.table(table);
            table.key_arity() == 0 && table.kind() != TableKind::Relation
        })
    };
    let mut rule = remove_globals(rule, global);
    for fact in &mut rule.body {
        match fact {
            Fact::Eq(_, a, b) => [a, b].into_iter().for_each(name_functions),
            Fact::Fact(expr) => name_functions(expr),
        }
    }
    for action in &mut rule.head.0 {
        match action {
            Action::Let(_, _, expr) | Action::Expr(_, expr) => name_functions(expr),
            Action::Set(_, _, args, value) => {
                args.iter_mut().chain([value]).for_each(name_functions)
            }
            Action::Change(_, _, _, args) => args.iter_mut().for_each(name_functions),
            Action::Union(_, a, b) => [a, b].into_iter().for_each(name_functions),
            Action::Panic(..) => {}
        }
    }
    let typed = typecheck(&rule, schema)?;
    Ok((rule, typed))
}

// `(unstable-fn "name" given...)` as the call `(@fn name given...)` that
// its targets are registered under.
fn name_functions(expr: &mut Expr) {
    if let Expr::Call(_, head, args) = expr {
        args.iter_mut().for_each(name_functions);
        if head == "unstable-fn"
            && let Some(Expr::Lit(_, Literal::String(name))) = args.first()
        {
            *head = format!("{FN_PREFIX}{name}");
            args.remove(0);
        }
    }
}

fn rewrite(lhs: Expr, rhs: Expr) -> Rewrite {
    Rewrite {
        span: lhs.span(),
        lhs,
        rhs,
        conditions: Vec::new(),
        name: String::new(),
    }
}

impl Schedule {
    /// One iteration of a ruleset.
    pub fn run(ruleset: &str) -> Self {
        let config = RunConfig {
            ruleset: ruleset.to_owned(),
            until: None,
        };
        Self::Run(span!(), config)
    }

    /// Repeats until an iteration changes nothing.
    pub fn saturate(schedule: impl Into<Self>) -> Self {
        Self::Saturate(span!(), Box::new(schedule.into()))
    }

    /// Repeats at most `times` times, stopping when nothing changes.
    pub fn repeat(times: usize, schedule: impl Into<Self>) -> Self {
        Self::Repeat(span!(), times, Box::new(schedule.into()))
    }

    pub fn seq(schedules: impl IntoIterator<Item = Self>) -> Self {
        Self::Sequence(span!(), schedules.into_iter().collect())
    }
}

/// The ruleset of this name, run once.
impl From<&str> for Schedule {
    fn from(ruleset: &str) -> Self {
        Self::run(ruleset)
    }
}
