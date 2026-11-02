// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/ast/desugar.rs and src/ast/remove_globals.rs. See libs/egglog/LICENSE-MIT.

use crate::Error;
use crate::ast::{
    Action, Change, Command, Expr, Fact, GenericActions, Rewrite, Rule, RuleEvalMode, Schema,
    Subdatatypes,
};

// The variable a desugared rewrite binds its left-hand side to.
const REWRITE_VAR: &str = "rewrite_var__";

// Rewrites a command into the commands it stands for: a datatype into its
// sort and constructors, a rewrite into a rule, and names each unnamed rule
// by its printed text. Other commands stay as they are.
pub(crate) fn desugar(command: Command) -> Result<Vec<Command>, Error> {
    Ok(match command {
        Command::Datatype {
            span,
            name,
            variants,
        } => {
            let sort = Command::Sort {
                span,
                name: name.clone(),
                presort_and_args: None,
            };
            let constructors = variants.into_iter().map(|v| constructor(v, &name));
            std::iter::once(sort).chain(constructors).collect()
        }
        Command::Datatypes { datatypes, .. } => {
            let (variants, sorts): (Vec<_>, Vec<_>) = datatypes
                .into_iter()
                .partition(|(.., sub)| matches!(sub, Subdatatypes::Variants(_)));
            let mut out: Vec<Command> = variants
                .iter()
                .map(|(span, name, _)| Command::Sort {
                    span: *span,
                    name: name.clone(),
                    presort_and_args: None,
                })
                .collect();
            for (span, name, sub) in sorts {
                if let Subdatatypes::NewSort(head, args) = sub {
                    let presort_and_args = Some((head, args));
                    out.push(Command::Sort {
                        span,
                        name,
                        presort_and_args,
                    });
                }
            }
            for (_, name, sub) in variants {
                if let Subdatatypes::Variants(variants) = sub {
                    out.extend(variants.into_iter().map(|v| constructor(v, &name)));
                }
            }
            out
        }
        Command::Rewrite(ruleset, rewrite, subsume) => {
            let name = rewrite_name(&rewrite, &ruleset, false, subsume);
            vec![rewrite_rule(ruleset, name, rewrite, subsume)?]
        }
        Command::BiRewrite(ruleset, rewrite) => {
            let name = rewrite_name(&rewrite, &ruleset, true, false);
            let mut reverse = rewrite.clone();
            std::mem::swap(&mut reverse.lhs, &mut reverse.rhs);
            let forward = rewrite_rule(ruleset.clone(), format!("{name}=>"), rewrite, false)?;
            let backward = rewrite_rule(ruleset, format!("{name}<="), reverse, false)?;
            vec![forward, backward]
        }
        Command::Rule { mut rule } => {
            if rule.name.is_empty() {
                rule.name = rule.to_string().replace('"', "'");
            }
            vec![Command::Rule { rule }]
        }
        Command::Fail(span, command) => {
            let mut out = desugar(*command)?;
            let Some(last) = out.pop() else {
                let message = "the command inside (fail ...) expands to no commands";
                return Err(Error::Desugar(span, message.to_owned()));
            };
            out.push(Command::Fail(span, Box::new(last)));
            out
        }
        command => vec![command],
    })
}

fn constructor(variant: crate::ast::Variant, sort: &str) -> Command {
    Command::Constructor {
        span: variant.span,
        name: variant.name,
        schema: Schema::new(variant.types, sort.to_owned()),
        cost: variant.cost,
        unextractable: variant.unextractable,
    }
}

fn rewrite_name(rewrite: &Rewrite, ruleset: &str, bidirectional: bool, subsume: bool) -> String {
    if !rewrite.name.is_empty() {
        return rewrite.name.clone();
    }
    struct Printed<'a>(&'a Rewrite, &'a str, bool, bool);
    impl std::fmt::Display for Printed<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt_with_ruleset(f, self.1, self.2, self.3)
        }
    }
    Printed(rewrite, ruleset, bidirectional, subsume)
        .to_string()
        .replace('"', "'")
}

// `(rewrite lhs rhs :when conditions)` matches `(= v lhs)` with the conditions
// and unions `v` with `rhs`; `:subsume` also subsumes the matched call.
fn rewrite_rule(
    ruleset: String,
    name: String,
    rewrite: Rewrite,
    subsume: bool,
) -> Result<Command, Error> {
    let span = rewrite.span;
    let var = Expr::Var(span, REWRITE_VAR.to_owned());
    let mut head = vec![Action::Union(span, var.clone(), rewrite.rhs)];
    if subsume {
        let Expr::Call(_, head_name, args) = &rewrite.lhs else {
            let message = "subsumed rewrite must have a function call on the lhs";
            return Err(Error::Desugar(rewrite.lhs.span(), message.to_owned()));
        };
        head.push(Action::Change(
            span,
            Change::Subsume,
            head_name.clone(),
            args.clone(),
        ));
    }
    let body = std::iter::once(Fact::Eq(span, var, rewrite.lhs))
        .chain(rewrite.conditions)
        .collect();
    Ok(Command::Rule {
        rule: Rule {
            span,
            head: GenericActions(head),
            body,
            name,
            ruleset,
            eval_mode: RuleEvalMode::Seminaive,
        },
    })
}

// Rewrites references to globals, which are nullary functions, into calls.
// The query calls them in place; the actions read a fresh variable that a
// new fact binds to the call.
pub(crate) fn remove_globals(mut rule: Rule, is_global: impl Fn(&str) -> bool) -> Rule {
    let mut bound: Vec<String> = Vec::new();
    for fact in &mut rule.body {
        match fact {
            Fact::Eq(_, a, b) => {
                call_globals(a, &is_global);
                call_globals(b, &is_global);
            }
            Fact::Fact(expr) | Fact::Not(expr) => call_globals(expr, &is_global),
        }
    }
    let mut read = |expr: &mut Expr| {
        let mut rename = |name: &mut String| {
            if is_global(name) {
                if !bound.contains(name) {
                    bound.push(name.clone());
                }
                *name = format!("{name}__global");
            }
        };
        rename_vars(expr, &mut rename);
    };
    for action in &mut rule.head.0 {
        match action {
            Action::Let(_, _, expr) | Action::Expr(_, expr) => read(expr),
            Action::Set(_, _, args, value) => {
                args.iter_mut().for_each(&mut read);
                read(value);
            }
            Action::Change(_, _, _, args) => args.iter_mut().for_each(&mut read),
            Action::Union(_, a, b) => {
                read(a);
                read(b);
            }
            Action::Panic(..) => {}
        }
    }
    let span = rule.span;
    rule.body.extend(bound.into_iter().map(|name| {
        let var = Expr::Var(span, format!("{name}__global"));
        Fact::Eq(span, var, Expr::Call(span, name, Vec::new()))
    }));
    rule
}

fn call_globals(expr: &mut Expr, is_global: &impl Fn(&str) -> bool) {
    match expr {
        Expr::Var(span, name) if is_global(name) => {
            *expr = Expr::Call(*span, std::mem::take(name), Vec::new());
        }
        Expr::Call(_, _, args) => args.iter_mut().for_each(|arg| call_globals(arg, is_global)),
        _ => {}
    }
}

fn rename_vars(expr: &mut Expr, rename: &mut impl FnMut(&mut String)) {
    match expr {
        Expr::Var(_, name) => rename(name),
        Expr::Call(_, _, args) => args.iter_mut().for_each(|arg| rename_vars(arg, rename)),
        Expr::Lit(..) => {}
    }
}
