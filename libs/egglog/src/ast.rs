// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/ast/mod.rs and egglog-ast/src/generic_ast.rs. See libs/egglog/LICENSE-MIT.

//! The egglog syntax tree: expressions, facts, actions, rules, schedules and
//! commands. `Display` prints each one as egglog text.

use std::fmt::{self, Display, Formatter};

use crate::F64;

/// Where a piece of syntax comes from: the Rust line that built it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: &'static str,
    pub line: u32,
}

impl Display for Span {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "At {} of {}", self.line, self.file)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Literal {
    Int(i64),
    Float(F64),
    String(String),
    Bool(bool),
    Unit,
    /// The `n`th argument of the run, of the sort its uses give it. See
    /// [`Program::run`](crate::Program::run).
    Arg(u32),
}

impl From<i64> for Literal {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<f64> for Literal {
    fn from(value: f64) -> Self {
        Self::Float(F64(value))
    }
}

impl From<String> for Literal {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Expr {
    Lit(Span, Literal),
    Var(Span, String),
    Call(Span, String, Vec<Expr>),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Self::Lit(span, _) | Self::Var(span, _) | Self::Call(span, ..) => *span,
        }
    }
}

/// A part of a rule's query: an expression that must exist, two that must
/// be equal, or a row that must be absent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Fact {
    Eq(Span, Expr, Expr),
    Fact(Expr),
    /// `(not (T args...))`: no row of the table `T` holds `args`, which are
    /// its key, or its key and output. A variable no other fact binds
    /// matches any cell. The rule must not run in a saturation that writes
    /// `T`, so the table is complete when the rule reads it.
    Not(Expr),
}

/// The query of a rule.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Facts(pub Vec<Fact>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Change {
    /// Remove the row.
    Delete,
    /// Hide the row from rules and extraction; lookups still see it.
    Subsume,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Bind a name: a local of the rule, or a global at the top level.
    Let(Span, String, Expr),
    /// Write the output of a function for a key.
    Set(Span, String, Vec<Expr>, Expr),
    Change(Span, Change, String, Vec<Expr>),
    /// Make two e-classes equal.
    Union(Span, Expr, Expr),
    Panic(Span, String),
    /// Evaluate an expression, making the terms it names.
    Expr(Span, Expr),
}

/// The actions of a rule.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct GenericActions(pub Vec<Action>);

pub type Actions = GenericActions;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RuleEvalMode {
    /// Match only what involves rows written since the ruleset last ran.
    #[default]
    Seminaive,
    /// Match the whole database every time.
    Naive,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rule {
    pub span: Span,
    pub head: Actions,
    pub body: Vec<Fact>,
    /// Unique among the rules of an e-graph; empty means the printed rule.
    pub name: String,
    pub ruleset: String,
    pub eval_mode: RuleEvalMode,
}

/// `lhs` becomes `rhs` where the conditions hold.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rewrite {
    pub span: Span,
    pub lhs: Expr,
    pub rhs: Expr,
    pub conditions: Vec<Fact>,
    pub name: String,
}

/// The sorts a function takes and the sort it returns.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Schema {
    pub input: Vec<String>,
    pub output: String,
}

impl Schema {
    pub fn new(input: Vec<String>, output: String) -> Self {
        Self { input, output }
    }
}

/// One constructor of a datatype.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Variant {
    pub span: Span,
    pub name: String,
    pub types: Vec<String>,
    pub cost: Option<u64>,
    pub unextractable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subdatatypes {
    Variants(Vec<Variant>),
    NewSort(String, Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunConfig {
    pub ruleset: String,
    pub until: Option<Vec<Fact>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Schedule {
    /// Repeat until an iteration changes nothing.
    Saturate(Span, Box<Schedule>),
    /// Repeat at most this many times, stopping early on no change.
    Repeat(Span, usize, Box<Schedule>),
    /// One iteration of a ruleset.
    Run(Span, RunConfig),
    Sequence(Span, Vec<Schedule>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrintFunctionMode {
    Default,
    CSV,
}

/// An egglog command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// A sort, made from a container sort and its arguments if given.
    Sort {
        span: Span,
        name: String,
        presort_and_args: Option<(String, Vec<Expr>)>,
    },
    Datatype {
        span: Span,
        name: String,
        variants: Vec<Variant>,
    },
    /// Datatypes that may refer to each other.
    Datatypes {
        span: Span,
        datatypes: Vec<(Span, String, Subdatatypes)>,
    },
    Constructor {
        span: Span,
        name: String,
        schema: Schema,
        cost: Option<u64>,
        unextractable: bool,
    },
    Relation {
        span: Span,
        name: String,
        inputs: Vec<String>,
    },
    /// A function; no merge means writing a second output is an error.
    Function {
        span: Span,
        name: String,
        schema: Schema,
        merge: Option<Expr>,
    },
    AddRuleset(Span, String),
    UnstableCombinedRuleset(Span, String, Vec<String>),
    Rule {
        rule: Rule,
    },
    /// A rewrite in a ruleset; `true` subsumes the left-hand side.
    Rewrite(String, Rewrite, bool),
    BiRewrite(String, Rewrite),
    Action(Action),
    /// Extract this many variants of the expression.
    Extract(Span, Expr, Expr),
    RunSchedule(Schedule),
    PrintOverallStatistics(Span, Option<String>),
    Check(Span, Vec<Fact>),
    PrintFunction(
        Span,
        String,
        Option<usize>,
        Option<String>,
        PrintFunctionMode,
    ),
    PrintSize(Span, Option<String>),
    /// The command must fail.
    Fail(Span, Box<Command>),
    UserDefined(Span, String, Vec<Expr>),
}

struct List<'a, T>(&'a [T], &'a str);

impl<T: Display> Display for List<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (i, item) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(self.1)?;
            }
            item.fmt(f)?;
        }
        Ok(())
    }
}

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(i) => Display::fmt(i, f),
            Self::Float(n) => {
                let text = n.0.to_string();
                if text.parse::<i64>().is_ok() {
                    write!(f, "{text}.0")
                } else {
                    f.write_str(&text)
                }
            }
            Self::Bool(b) => Display::fmt(b, f),
            Self::String(s) => {
                f.write_str("\"")?;
                for c in s.chars() {
                    match c {
                        '\\' => f.write_str("\\\\")?,
                        '"' => f.write_str("\\\"")?,
                        c => write!(f, "{c}")?,
                    }
                }
                f.write_str("\"")
            }
            Self::Unit => f.write_str("()"),
            Self::Arg(n) => write!(f, "${n}"),
        }
    }
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lit(_, lit) => write!(f, "{lit}"),
            Self::Var(_, var) => f.write_str(var),
            Self::Call(_, op, args) if args.is_empty() => write!(f, "({op})"),
            Self::Call(_, op, args) => write!(f, "({op} {})", List(args, " ")),
        }
    }
}

impl Display for Fact {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eq(_, a, b) => write!(f, "(= {a} {b})"),
            Self::Fact(expr) => write!(f, "{expr}"),
            Self::Not(expr) => write!(f, "(not {expr})"),
        }
    }
}

// `(head)` or `(head args...)`.
fn call(f: &mut Formatter<'_>, head: &str, args: &[Expr]) -> fmt::Result {
    if args.is_empty() {
        write!(f, "({head})")
    } else {
        write!(f, "({head} {})", List(args, " "))
    }
}

impl Display for Action {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Let(_, name, value) => write!(f, "(let {name} {value})"),
            Self::Set(_, head, args, value) => {
                f.write_str("(set ")?;
                call(f, head, args)?;
                write!(f, " {value})")
            }
            Self::Union(_, a, b) => write!(f, "(union {a} {b})"),
            Self::Change(_, change, head, args) => {
                let change = match change {
                    Change::Delete => "delete",
                    Change::Subsume => "subsume",
                };
                write!(f, "({change} ")?;
                call(f, head, args)?;
                f.write_str(")")
            }
            Self::Panic(_, message) => write!(f, "(panic {})", Literal::String(message.clone())),
            Self::Expr(_, expr) => write!(f, "{expr}"),
        }
    }
}

impl Display for Rule {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let indent = " ".repeat(7);
        let lines = |f: &mut Formatter<'_>, items: &[String]| -> fmt::Result {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(&indent)?;
                }
                f.write_str(item)?;
                if i + 1 != items.len() {
                    writeln!(f)?;
                }
            }
            Ok(())
        };
        f.write_str("(rule (")?;
        lines(
            f,
            &self
                .body
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        )?;
        f.write_str(")\n      (")?;
        lines(
            f,
            &self
                .head
                .0
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        )?;
        let ruleset = if self.ruleset.is_empty() {
            String::new()
        } else {
            format!(":ruleset {}", self.ruleset)
        };
        let name = if self.name.is_empty() {
            String::new()
        } else {
            format!(":name {}", Literal::String(self.name.clone()))
        };
        let eval_mode = match self.eval_mode {
            RuleEvalMode::Seminaive => "",
            RuleEvalMode::Naive => " :naive",
        };
        write!(f, ")\n{indent} {ruleset} {name}{eval_mode})")
    }
}

impl Rewrite {
    /// Prints the rewrite as the command that declares it.
    pub fn fmt_with_ruleset(
        &self,
        f: &mut Formatter<'_>,
        ruleset: &str,
        is_bidirectional: bool,
        subsume: bool,
    ) -> fmt::Result {
        let direction = if is_bidirectional {
            "birewrite"
        } else {
            "rewrite"
        };
        write!(f, "({direction} {} {}", self.lhs, self.rhs)?;
        if subsume {
            f.write_str(" :subsume")?;
        }
        if !self.conditions.is_empty() {
            write!(f, " :when ({})", List(&self.conditions, " "))?;
        }
        if !ruleset.is_empty() {
            write!(f, " :ruleset {ruleset}")?;
        }
        f.write_str(")")
    }
}

impl Display for Schema {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "({}) {}", List(&self.input, " "), self.output)
    }
}

impl Display for Variant {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "({}", self.name)?;
        if !self.types.is_empty() {
            write!(f, " {}", List(&self.types, " "))?;
        }
        if let Some(cost) = self.cost {
            write!(f, " :cost {cost}")?;
        }
        f.write_str(")")
    }
}

impl Display for RunConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("(run")?;
        if !self.ruleset.is_empty() {
            write!(f, " {}", self.ruleset)?;
        }
        if let Some(until) = &self.until {
            write!(f, " :until {}", List(until, " "))?;
        }
        f.write_str(")")
    }
}

impl Display for Schedule {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Saturate(_, schedule) => write!(f, "(saturate {schedule})"),
            Self::Repeat(_, n, schedule) => write!(f, "(repeat {n} {schedule})"),
            Self::Run(_, config) => write!(f, "{config}"),
            Self::Sequence(_, schedules) => write!(f, "(seq {})", List(schedules, " ")),
        }
    }
}

impl Display for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let string = |s: &String| Literal::String(s.clone());
        match self {
            Self::Sort {
                name,
                presort_and_args: None,
                ..
            } => write!(f, "(sort {name})"),
            Self::Sort {
                name,
                presort_and_args: Some((head, args)),
                ..
            } => write!(f, "(sort {name} ({head} {}))", List(args, " ")),
            Self::Datatype { name, variants, .. } => {
                write!(f, "(datatype {name} {})", List(variants, " "))
            }
            Self::Datatypes { datatypes, .. } => {
                let datatypes: Vec<String> = datatypes
                    .iter()
                    .map(|(_, name, sub)| match sub {
                        Subdatatypes::Variants(variants) => {
                            format!("({name} {})", List(variants, " "))
                        }
                        Subdatatypes::NewSort(head, args) => {
                            format!("(sort {name} ({head} {}))", List(args, " "))
                        }
                    })
                    .collect();
                write!(f, "(datatype* {})", List(&datatypes, " "))
            }
            Self::Constructor {
                name,
                schema,
                cost,
                unextractable,
                ..
            } => {
                write!(f, "(constructor {name} {schema}")?;
                if let Some(cost) = cost {
                    write!(f, " :cost {cost}")?;
                }
                if *unextractable {
                    f.write_str(" :unextractable")?;
                }
                f.write_str(")")
            }
            Self::Relation { name, inputs, .. } => {
                write!(f, "(relation {name} ({}))", List(inputs, " "))
            }
            Self::Function {
                name,
                schema,
                merge,
                ..
            } => match merge {
                Some(merge) => write!(f, "(function {name} {schema} :merge {merge})"),
                None => write!(f, "(function {name} {schema} :no-merge)"),
            },
            Self::AddRuleset(_, name) => write!(f, "(ruleset {name})"),
            Self::UnstableCombinedRuleset(_, name, others) => {
                write!(
                    f,
                    "(unstable-combined-ruleset {name} {})",
                    List(others, " ")
                )
            }
            Self::Rule { rule } => rule.fmt(f),
            Self::Rewrite(ruleset, rewrite, subsume) => {
                rewrite.fmt_with_ruleset(f, ruleset, false, *subsume)
            }
            Self::BiRewrite(ruleset, rewrite) => rewrite.fmt_with_ruleset(f, ruleset, true, false),
            Self::Action(action) => write!(f, "{action}"),
            Self::Extract(_, expr, variants) => write!(f, "(extract {expr} {variants})"),
            Self::RunSchedule(schedule) => write!(f, "(run-schedule {schedule})"),
            Self::PrintOverallStatistics(_, None) => f.write_str("(print-stats)"),
            Self::PrintOverallStatistics(_, Some(file)) => {
                write!(f, "(print-stats :file {})", string(file))
            }
            Self::Check(_, facts) => write!(f, "(check {})", List(facts, "\n")),
            Self::PrintFunction(_, name, n, file, mode) => {
                write!(f, "(print-function {name}")?;
                if let Some(n) = n {
                    write!(f, " {n}")?;
                }
                if let Some(file) = file {
                    write!(f, " :file {}", string(file))?;
                }
                if *mode == PrintFunctionMode::CSV {
                    f.write_str(" :mode csv")?;
                }
                f.write_str(")")
            }
            Self::PrintSize(_, name) => write!(f, "(print-size {})", name.as_deref().unwrap_or("")),
            Self::Fail(_, command) => write!(f, "(fail {command})"),
            Self::UserDefined(_, name, args) => write!(f, "({name} {})", List(args, " ")),
        }
    }
}
