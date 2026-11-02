//! egglog programs in the syntax of upstream egglog's parser
//! (`src/ast/parse.rs`), for the commands this crate runs. Each token parser
//! eats the whitespace and comments after it.

use std::collections::HashSet;

use chumsky::error::{RichPattern, RichReason};
use chumsky::input::MapExtra;
use chumsky::inspector::SimpleState;
use chumsky::prelude::*;

use crate::ast::{
    Action, Change, Command, Expr, Fact, GenericActions, Literal, PrintFunctionMode, Rewrite, Rule,
    RuleEvalMode, RunConfig, Schedule, Schema, Span, Subdatatypes, Variant,
};
use crate::{EGraph, Error, F64};

// The heads a list must not have to read as a call, so that a malformed
// command or action reports its own error.
const KEYWORDS: &[&str] = &[
    "sort",
    "datatype",
    "datatype*",
    "function",
    "constructor",
    "relation",
    "ruleset",
    "unstable-combined-ruleset",
    "rule",
    "rewrite",
    "birewrite",
    "run",
    "run-schedule",
    "extract",
    "check",
    "print-stats",
    "print-function",
    "print-size",
    "fail",
    "let",
    "set",
    "delete",
    "subsume",
    "union",
    "panic",
    "=",
];

const ACTION_KEYWORDS: &[&str] = &["let", "set", "delete", "subsume", "union", "panic"];

struct Source<'a> {
    file: &'static str,
    // The offset each line starts at.
    lines: Vec<usize>,
    // The user-defined commands of the e-graph.
    commands: Vec<String>,
    // Each `_` is a variable of its own.
    wildcards: usize,
    names: HashSet<&'a str, tir_adt::FxBuildHasher>,
}

impl Source<'_> {
    fn wildcard(&mut self) -> String {
        loop {
            self.wildcards += 1;
            let name = format!("_{}", self.wildcards);
            if !self.names.contains(name.as_str()) {
                return name;
            }
        }
    }

    fn at(&self, offset: usize) -> Span {
        Span {
            file: self.file,
            line: self.lines.partition_point(|&start| start <= offset) as u32,
        }
    }
}

type Extra<'a> = extra::Full<Rich<'a, char>, SimpleState<Source<'a>>, ()>;

fn span<'a>(e: &mut MapExtra<'a, '_, &'a str, Extra<'a>>) -> Span {
    let start = e.span().start;
    e.state().at(start)
}

fn ws<'a>() -> impl Parser<'a, &'a str, (), Extra<'a>> + Clone {
    let comment = just(';').then(none_of('\n').repeated()).ignored();
    choice((
        any().filter(|c: &char| c.is_whitespace()).ignored(),
        comment,
    ))
    .repeated()
    .ignored()
}

fn open<'a>() -> impl Parser<'a, &'a str, (), Extra<'a>> + Clone {
    just('(').then_ignore(ws()).ignored()
}

fn close<'a>() -> impl Parser<'a, &'a str, (), Extra<'a>> + Clone {
    just(')').then_ignore(ws()).ignored()
}

fn separator(c: char) -> bool {
    c.is_whitespace() || "();\"".contains(c)
}

fn atom<'a>() -> impl Parser<'a, &'a str, &'a str, Extra<'a>> + Clone {
    any()
        .filter(|c: &char| !separator(*c))
        .repeated()
        .at_least(1)
        .to_slice()
        .then_ignore(ws())
}

fn literal(atom: &str) -> Option<Literal> {
    Some(match atom {
        "true" => Literal::Bool(true),
        "false" => Literal::Bool(false),
        "NaN" => Literal::Float(F64(f64::NAN)),
        "inf" => Literal::Float(F64(f64::INFINITY)),
        "-inf" => Literal::Float(F64(f64::NEG_INFINITY)),
        atom => match atom.parse::<i64>() {
            Ok(int) => Literal::Int(int),
            Err(_) => Literal::Float(F64(atom.parse().ok().filter(|f: &f64| f.is_finite())?)),
        },
    })
}

// An atom that is no literal and no option.
fn symbol<'a>() -> impl Parser<'a, &'a str, String, Extra<'a>> + Clone {
    atom()
        .filter(|atom: &&str| literal(atom).is_none() && !atom.starts_with(':'))
        .map(str::to_owned)
        .labelled("a name")
}

fn key<'a>(word: &'static str) -> impl Parser<'a, &'a str, (), Extra<'a>> + Clone {
    atom()
        .filter(move |atom| *atom == word)
        .ignored()
        .labelled(word)
}

fn string<'a>() -> impl Parser<'a, &'a str, String, Extra<'a>> + Clone {
    let escape = just('\\').ignore_then(choice((
        just('n').to('\n'),
        just('t').to('\t'),
        just('\\'),
        just('"'),
    )));
    choice((escape, none_of("\\\"")))
        .repeated()
        .collect()
        .delimited_by(just('"'), just('"'))
        .then_ignore(ws())
        .labelled("a string")
}

fn uint<'a, T: std::str::FromStr>() -> impl Parser<'a, &'a str, T, Extra<'a>> + Clone {
    atom()
        .filter(|atom: &&str| atom.bytes().all(|b| b.is_ascii_digit()))
        .try_map(|atom, span| (atom.parse()).map_err(|_| Rich::custom(span, "too large")))
        .labelled("a number")
}

// `(head body)`.
fn form<'a, O>(
    head: &'static str,
    body: impl Parser<'a, &'a str, O, Extra<'a>> + Clone,
) -> impl Parser<'a, &'a str, O, Extra<'a>> + Clone {
    open()
        .ignore_then(key(head))
        .ignore_then(body)
        .then_ignore(close())
}

fn list<'a, O>(
    item: impl Parser<'a, &'a str, O, Extra<'a>> + Clone,
) -> impl Parser<'a, &'a str, Vec<O>, Extra<'a>> + Clone {
    item.repeated().collect().delimited_by(open(), close())
}

// `(name expr*)`, as a table row or a container sort.
fn call<'a>() -> impl Parser<'a, &'a str, (String, Vec<Expr>), Extra<'a>> + Clone {
    open()
        .ignore_then(symbol())
        .then(expr().repeated().collect())
        .then_ignore(close())
}

// A list whose head is no keyword.
fn plain<'a>(
    excluded: &'static [&'static str],
) -> impl Parser<'a, &'a str, Expr, Extra<'a>> + Clone {
    let head = open().then(atom().filter(move |atom| !excluded.contains(atom)));
    head.rewind().ignore_then(expr())
}

fn expr<'a>() -> impl Parser<'a, &'a str, Expr, Extra<'a>> + Clone {
    recursive(|expr| {
        let string = string().map_with(|text, e| Expr::Lit(span(e), Literal::String(text)));
        let atom = atom()
            .filter(|atom: &&str| !atom.starts_with(':'))
            .map_with(|atom, e| match literal(atom) {
                Some(literal) => Expr::Lit(span(e), literal),
                None if atom == "_" => {
                    let name = e.state().wildcard();
                    Expr::Var(span(e), name)
                }
                None => Expr::Var(span(e), atom.to_owned()),
            });
        let unit = open()
            .then(close())
            .map_with(|_, e| Expr::Lit(span(e), Literal::Unit));
        let call = open()
            .ignore_then(symbol())
            .then(expr.repeated().collect())
            .then_ignore(close())
            .map_with(|(head, args), e| Expr::Call(span(e), head, args));
        choice((string, atom, unit, call)).labelled("an expression")
    })
}

fn fact<'a>() -> impl Parser<'a, &'a str, Fact, Extra<'a>> + Clone {
    choice((
        form("=", expr().then(expr())).map_with(|(a, b), e| Fact::Eq(span(e), a, b)),
        plain(&["="]).map(Fact::Fact),
    ))
    .labelled("a fact")
}

fn action<'a>(
    excluded: &'static [&'static str],
) -> impl Parser<'a, &'a str, Action, Extra<'a>> + Clone {
    let change = |head, change| {
        form(head, call())
            .map_with(move |(table, args), e| Action::Change(span(e), change, table, args))
    };
    choice((
        form("let", symbol().then(expr()))
            .map_with(|(name, value), e| Action::Let(span(e), name, value)),
        form("set", call().then(expr()))
            .map_with(|((table, args), value), e| Action::Set(span(e), table, args, value)),
        change("delete", Change::Delete),
        change("subsume", Change::Subsume),
        form("union", expr().then(expr())).map_with(|(a, b), e| Action::Union(span(e), a, b)),
        form("panic", string()).map_with(|message, e| Action::Panic(span(e), message)),
        plain(excluded).map_with(|expr, e| Action::Expr(span(e), expr)),
    ))
    .labelled("an action")
}

// The `:key value` options after the operands of a command.
#[derive(Clone)]
enum Opt {
    Ruleset(String),
    Name(String),
    Naive,
    Subsume,
    When(Vec<Fact>),
    Until(Vec<Fact>),
    Merge(Expr),
    NoMerge,
    Cost(u64),
    Unextractable,
    File(String),
    Mode(PrintFunctionMode),
}

// The options `allowed`, in any order.
fn options<'a>(
    allowed: &'static [&'static str],
) -> impl Parser<'a, &'a str, Vec<Opt>, Extra<'a>> + Clone {
    let options: Vec<_> = allowed.iter().map(|&word| option(word)).collect();
    choice(options).repeated().collect()
}

fn option<'a>(word: &'static str) -> Boxed<'a, 'a, &'a str, Opt, Extra<'a>> {
    let mode = choice((
        key("default").to(PrintFunctionMode::Default),
        key("csv").to(PrintFunctionMode::CSV),
    ));
    let value = match word {
        ":ruleset" => symbol().map(Opt::Ruleset).boxed(),
        ":name" => choice((string(), symbol())).map(Opt::Name).boxed(),
        ":naive" => empty().to(Opt::Naive).boxed(),
        ":subsume" => empty().to(Opt::Subsume).boxed(),
        ":when" => list(fact()).map(Opt::When).boxed(),
        ":until" => fact().repeated().collect().map(Opt::Until).boxed(),
        ":merge" => expr().map(Opt::Merge).boxed(),
        ":no-merge" => empty().to(Opt::NoMerge).boxed(),
        ":cost" => uint().map(Opt::Cost).boxed(),
        ":unextractable" => empty().to(Opt::Unextractable).boxed(),
        ":file" => string().map(Opt::File).boxed(),
        _ => mode.map(Opt::Mode).boxed(),
    };
    key(word).ignore_then(value).boxed()
}

fn until(options: Vec<Opt>) -> Option<Vec<Fact>> {
    options.into_iter().find_map(|option| match option {
        Opt::Until(facts) => Some(facts),
        _ => None,
    })
}

fn file(options: &[Opt]) -> Option<String> {
    options.iter().find_map(|option| match option {
        Opt::File(file) => Some(file.clone()),
        _ => None,
    })
}

fn run(ruleset: Option<String>, until: Option<Vec<Fact>>) -> RunConfig {
    RunConfig {
        ruleset: ruleset.unwrap_or_default(),
        until,
    }
}

fn schedule<'a>() -> impl Parser<'a, &'a str, Schedule, Extra<'a>> + Clone {
    recursive(|schedule| {
        let schedules = schedule.repeated().collect::<Vec<_>>();
        let seq = |span, schedules| Box::new(Schedule::Sequence(span, schedules));
        choice((
            form("saturate", schedules.clone()).map_with(move |schedules, e| {
                let span = span(e);
                Schedule::Saturate(span, seq(span, schedules))
            }),
            form("seq", schedules.clone())
                .map_with(|schedules, e| Schedule::Sequence(span(e), schedules)),
            form("repeat", uint().then(schedules)).map_with(move |(n, schedules), e| {
                let span = span(e);
                Schedule::Repeat(span, n, seq(span, schedules))
            }),
            form("run", symbol().or_not().then(options(&[":until"]))).map_with(
                |(ruleset, options), e| Schedule::Run(span(e), run(ruleset, until(options))),
            ),
            symbol().map_with(|ruleset, e| Schedule::Run(span(e), run(Some(ruleset), None))),
        ))
        .labelled("a schedule")
    })
}

fn variant<'a>() -> impl Parser<'a, &'a str, Variant, Extra<'a>> + Clone {
    open()
        .ignore_then(symbol())
        .then(symbol().repeated().collect())
        .then(options(&[":cost", ":unextractable"]))
        .then_ignore(close())
        .map_with(|((name, types), options), e| {
            let mut variant = Variant {
                span: span(e),
                name,
                types,
                cost: None,
                unextractable: false,
            };
            for option in options {
                match option {
                    Opt::Cost(cost) => variant.cost = Some(cost),
                    _ => variant.unextractable = true,
                }
            }
            variant
        })
}

fn datatype<'a>() -> impl Parser<'a, &'a str, (Span, String, Subdatatypes), Extra<'a>> + Clone {
    choice((
        form("sort", symbol().then(call())).map_with(|(name, (presort, args)), e| {
            (span(e), name, Subdatatypes::NewSort(presort, args))
        }),
        open()
            .ignore_then(symbol())
            .then(variant().repeated().collect())
            .then_ignore(close())
            .map_with(|(name, variants), e| (span(e), name, Subdatatypes::Variants(variants))),
    ))
}

fn declaration<'a>() -> impl Parser<'a, &'a str, Command, Extra<'a>> + Clone {
    let sorts = || list(symbol());
    choice((
        form("sort", symbol().then(call().or_not())).map_with(|(name, presort_and_args), e| {
            Command::Sort {
                span: span(e),
                name,
                presort_and_args,
            }
        }),
        form("datatype", symbol().then(variant().repeated().collect())).map_with(
            |(name, variants), e| Command::Datatype {
                span: span(e),
                name,
                variants,
            },
        ),
        form("datatype*", datatype().repeated().collect()).map_with(|datatypes, e| {
            Command::Datatypes {
                span: span(e),
                datatypes,
            }
        }),
        form(
            "function",
            symbol()
                .then(sorts())
                .then(symbol())
                .then(options(&[":merge", ":no-merge"])),
        )
        .try_map_with(|(((name, input), output), options), e| {
            let no_merge = options.iter().any(|option| matches!(option, Opt::NoMerge));
            let merge = options.into_iter().find_map(|option| match option {
                Opt::Merge(merge) => Some(merge),
                _ => None,
            });
            if no_merge && merge.is_some() {
                return Err(Rich::custom(e.span(), "conflicting merge annotations"));
            }
            Ok(Command::Function {
                span: span(e),
                name,
                schema: Schema::new(input, output),
                merge,
            })
        }),
        form(
            "constructor",
            symbol()
                .then(sorts())
                .then(symbol())
                .then(options(&[":cost", ":unextractable"])),
        )
        .map_with(
            |(((name, input), output), options), e| Command::Constructor {
                span: span(e),
                name,
                schema: Schema::new(input, output),
                cost: options.iter().find_map(|option| match option {
                    Opt::Cost(cost) => Some(*cost),
                    _ => None,
                }),
                unextractable: options
                    .iter()
                    .any(|option| matches!(option, Opt::Unextractable)),
            },
        ),
        form("relation", symbol().then(sorts())).map_with(|(name, inputs), e| Command::Relation {
            span: span(e),
            name,
            inputs,
        }),
        form("ruleset", symbol()).map_with(|name, e| Command::AddRuleset(span(e), name)),
        form(
            "unstable-combined-ruleset",
            symbol().then(symbol().repeated().collect()),
        )
        .map_with(|(name, members), e| Command::UnstableCombinedRuleset(span(e), name, members)),
    ))
}

fn rules<'a>() -> impl Parser<'a, &'a str, Command, Extra<'a>> + Clone {
    let rewrite = |head, allowed| {
        form(head, expr().then(expr()).then(options(allowed))).map_with(
            |((lhs, rhs), options), e| {
                let mut rewrite = Rewrite {
                    span: span(e),
                    lhs,
                    rhs,
                    conditions: Vec::new(),
                    name: String::new(),
                };
                let (mut ruleset, mut subsume) = (String::new(), false);
                for option in options {
                    match option {
                        Opt::Ruleset(name) => ruleset = name,
                        Opt::Name(name) => rewrite.name = name,
                        Opt::When(facts) => rewrite.conditions = facts,
                        _ => subsume = true,
                    }
                }
                (ruleset, rewrite, subsume)
            },
        )
    };
    let rule = form(
        "rule",
        list(fact())
            .then(list(action(ACTION_KEYWORDS)))
            .then(options(&[":ruleset", ":name", ":naive"])),
    )
    .map_with(|((body, head), options), e| {
        let mut rule = Rule {
            span: span(e),
            head: GenericActions(head),
            body,
            name: String::new(),
            ruleset: String::new(),
            eval_mode: RuleEvalMode::Seminaive,
        };
        for option in options {
            match option {
                Opt::Ruleset(name) => rule.ruleset = name,
                Opt::Name(name) => rule.name = name,
                _ => rule.eval_mode = RuleEvalMode::Naive,
            }
        }
        Command::Rule { rule }
    });
    choice((
        rule,
        rewrite("rewrite", &[":ruleset", ":name", ":when", ":subsume"])
            .map(|(ruleset, rewrite, subsume)| Command::Rewrite(ruleset, rewrite, subsume)),
        rewrite("birewrite", &[":ruleset", ":name", ":when"])
            .map(|(ruleset, rewrite, _)| Command::BiRewrite(ruleset, rewrite)),
    ))
}

fn queries<'a>() -> impl Parser<'a, &'a str, Command, Extra<'a>> + Clone {
    choice((
        form(
            "run",
            symbol().or_not().then(uint()).then(options(&[":until"])),
        )
        .map_with(|((ruleset, n), options), e| {
            let span = span(e);
            let run = Schedule::Run(span, run(ruleset, until(options)));
            Command::RunSchedule(Schedule::Repeat(span, n, Box::new(run)))
        }),
        form("run-schedule", schedule().repeated().collect())
            .map_with(|schedules, e| Command::RunSchedule(Schedule::Sequence(span(e), schedules))),
        form("extract", expr().then(expr().or_not())).map_with(|(expr, variants), e| {
            let span = span(e);
            let variants = variants.unwrap_or(Expr::Lit(span, Literal::Int(0)));
            Command::Extract(span, expr, variants)
        }),
        form("check", fact().repeated().collect())
            .map_with(|facts, e| Command::Check(span(e), facts)),
        form("print-stats", options(&[":file"]))
            .map_with(|options, e| Command::PrintOverallStatistics(span(e), file(&options))),
        form(
            "print-function",
            symbol()
                .then(uint().or_not())
                .then(options(&[":file", ":mode"])),
        )
        .map_with(|((name, rows), options), e| {
            let mode = options.iter().find_map(|option| match option {
                Opt::Mode(mode) => Some(*mode),
                _ => None,
            });
            let mode = mode.unwrap_or(PrintFunctionMode::Default);
            Command::PrintFunction(span(e), name, rows, file(&options), mode)
        }),
        form("print-size", symbol().or_not()).map_with(|name, e| Command::PrintSize(span(e), name)),
    ))
}

fn command<'a>() -> impl Parser<'a, &'a str, Command, Extra<'a>> + Clone {
    recursive(|command| {
        let user = open()
            .ignore_then(
                symbol()
                    .map_with(|name, e| (e.state().commands.contains(&name), name))
                    .filter(|(registered, _)| *registered)
                    .map(|(_, name)| name),
            )
            .then(expr().repeated().collect())
            .then_ignore(close())
            .map_with(|(name, args), e| Command::UserDefined(span(e), name, args));
        choice((
            user,
            declaration(),
            rules(),
            queries(),
            form("fail", command).map_with(|command, e| Command::Fail(span(e), Box::new(command))),
            action(KEYWORDS).map(Command::Action),
        ))
        .labelled("a command")
    })
}

impl EGraph {
    /// The commands of `text` in egglog's syntax, their spans in `file`.
    /// Text that stops inside a command is [`Error::Unfinished`].
    pub fn parse_program(&self, file: &'static str, text: &str) -> Result<Vec<Command>, Error> {
        let newlines = text.match_indices('\n').map(|(at, _)| at + 1);
        let mut source = SimpleState(Source {
            file,
            lines: std::iter::once(0).chain(newlines).collect(),
            commands: self.commands.keys().cloned().collect(),
            wildcards: 0,
            names: text
                .split(separator)
                .flat_map(|name| {
                    [
                        name,
                        name.strip_prefix(crate::GLOBAL_NAME_PREFIX).unwrap_or(name),
                    ]
                })
                .chain(self.names.seen.keys().map(String::as_str))
                .chain(self.names.aliases.keys().map(String::as_str))
                .collect(),
        });
        let program = ws()
            .ignore_then(command().repeated().collect())
            .then_ignore(end());
        let result = program.parse_with_state(text, &mut source).into_result();
        result.map_err(|errors| {
            let error = &errors[0];
            let at = source.at(error.span().start);
            match error.reason() {
                RichReason::Custom(message) => Error::Parse(format!("{at}: {message}")),
                _ if error.found().is_none() => Error::Unfinished(at),
                RichReason::ExpectedFound { expected, found } => {
                    // Whitespace and comments may follow any token.
                    let expected: Vec<String> = (expected.iter())
                        .filter(|pattern| match pattern {
                            RichPattern::Token(token) => **token != ';',
                            pattern => !matches!(pattern, RichPattern::SomethingElse),
                        })
                        .map(ToString::to_string)
                        .collect();
                    let found = found.as_deref().copied().unwrap_or_default();
                    let expected = expected.join(", ");
                    Error::Parse(format!("{at}: expected {expected}, found '{found}'"))
                }
            }
        })
    }
}
