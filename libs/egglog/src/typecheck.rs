// Sort inference over the database schema. Every variable starts with an
// unknown sort; a table atom fixes the sorts of its columns, and each
// primitive call keeps the overloads that agree with the sorts known so far,
// until nothing changes. A call left with no single overload, or a variable
// with no sort, is an error. The result is the flattened rule: the query as
// table and primitive atoms over variables, constants as variables with a
// value, and the actions with nested calls flattened.

use std::collections::HashMap;

use tir_adt::FxBuildHasher;

use crate::ast::{self, Action, Change, Expr, Fact, Literal, RuleEvalMode, Span};
use crate::plan::hir::{Act, Atom, Rule, Target, Var};
use crate::{Context, Error, ExternId, Schema, SortId, TableId, TableKind};

// The action that finds or makes a group of rows.
const FIND_OR_MAKE: &str = "find-or-make";

// Where a call sits: in the query, among the actions, or as an action whose
// result goes unused.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Query,
    Action,
    Effect,
}

struct Call {
    span: Span,
    name: String,
    overloads: Vec<ExternId>,
    args: Vec<Var>,
}

enum Item {
    Atom(TableId, Vec<Var>),
    Call(usize),
}

enum Do {
    Act(Act),
    Prim(usize),
    Effect(usize),
}

struct Typer<'a> {
    schema: Schema<'a>,
    span: Span,
    // Where the query and the actions run, which limits the primitives.
    contexts: (Context, Context),
    names: Vec<String>,
    constants: Vec<Option<Literal>>,
    sorts: Vec<Option<SortId>>,
    // Variables the query equates point at one representative.
    parent: Vec<Var>,
    bound: HashMap<String, Var, FxBuildHasher>,
    literals: HashMap<Literal, Var, FxBuildHasher>,
    query: Vec<Item>,
    actions: Vec<Do>,
    calls: Vec<Call>,
}

pub(crate) fn typecheck(rule: &ast::Rule, schema: Schema<'_>) -> Result<Rule, Error> {
    let contexts = match rule.eval_mode {
        RuleEvalMode::Naive => (Context::Read, Context::Full),
        RuleEvalMode::Seminaive => (Context::Pure, Context::Write),
    };
    let mut typer = Typer {
        schema,
        span: rule.span,
        contexts,
        names: Vec::new(),
        constants: Vec::new(),
        sorts: Vec::new(),
        parent: Vec::new(),
        bound: HashMap::default(),
        literals: HashMap::default(),
        query: Vec::new(),
        actions: Vec::new(),
        calls: Vec::new(),
    };
    for fact in &rule.body {
        match fact {
            Fact::Eq(span, a, b) => {
                let (a, b) = (typer.term(a, Place::Query)?, typer.term(b, Place::Query)?);
                typer.equate(*span, a, b)?;
            }
            Fact::Fact(expr) => _ = typer.term(expr, Place::Query)?,
        }
    }
    for action in &rule.head.0 {
        typer.action(action)?;
    }
    typer.infer()?;
    Ok(typer.finish(rule))
}

impl Typer<'_> {
    fn var(&mut self, name: String, constant: Option<Literal>, sort: Option<SortId>) -> Var {
        let var = self.names.len() as Var;
        self.names.push(name);
        self.constants.push(constant);
        self.sorts.push(sort);
        self.parent.push(var);
        var
    }

    fn find(&self, mut var: Var) -> Var {
        while self.parent[var as usize] != var {
            var = self.parent[var as usize];
        }
        var
    }

    fn literal(&mut self, literal: &Literal) -> Var {
        if let Some(&var) = self.literals.get(literal) {
            return var;
        }
        let sort = match literal {
            Literal::Int(_) => "i64",
            Literal::Float(_) => "f64",
            Literal::String(_) => "String",
            Literal::Bool(_) => "bool",
            Literal::Unit => "Unit",
        };
        let sort = self.schema.sort_named(sort);
        let var = self.var(literal.to_string(), Some(literal.clone()), sort);
        self.literals.insert(literal.clone(), var);
        var
    }

    fn type_error(&self, span: Span, message: String) -> Error {
        Error::Type(span, format!("{message} in rule {}", self.span))
    }

    fn equate(&mut self, span: Span, a: Var, b: Var) -> Result<(), Error> {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return Ok(());
        }
        let (keep, drop) = match (&self.constants[a as usize], &self.constants[b as usize]) {
            (Some(_), Some(_)) => {
                let message = "the query equates two different constants".to_owned();
                return Err(self.type_error(span, message));
            }
            (None, Some(_)) => (b, a),
            _ => (a, b),
        };
        self.parent[drop as usize] = keep;
        if let Some(sort) = self.sorts[drop as usize] {
            self.fix(keep, sort)?;
        }
        Ok(())
    }

    fn term(&mut self, expr: &Expr, place: Place) -> Result<Var, Error> {
        match expr {
            Expr::Lit(_, literal) => Ok(self.literal(literal)),
            Expr::Var(span, name) => match self.bound.get(name) {
                Some(&var) => Ok(self.find(var)),
                None if place == Place::Query => {
                    let var = self.var(name.clone(), None, None);
                    self.bound.insert(name.clone(), var);
                    Ok(var)
                }
                None => Err(self.type_error(*span, format!("unbound variable `{name}`"))),
            },
            Expr::Call(span, name, rows) if name == FIND_OR_MAKE => {
                self.find_or_make(*span, rows, place)
            }
            Expr::Call(span, name, args) => {
                let inner = if place == Place::Query {
                    Place::Query
                } else {
                    Place::Action
                };
                let args = (args.iter())
                    .map(|arg| self.term(arg, inner))
                    .collect::<Result<_, _>>()?;
                self.call(*span, name, args, place)
            }
        }
    }

    fn call(
        &mut self,
        span: Span,
        name: &str,
        mut args: Vec<Var>,
        place: Place,
    ) -> Result<Var, Error> {
        if let Some(table) = self.schema.table_named(name) {
            let schema = self.schema.table(table);
            if args.len() != schema.key_arity() {
                let message = format!("`{name}` takes {} arguments", schema.key_arity());
                return Err(self.type_error(span, message));
            }
            let kind = schema.kind();
            let out = if kind == TableKind::Relation {
                self.literal(&Literal::Unit)
            } else {
                let out = self.var(format!("({name} ...)"), None, None);
                args.push(out);
                out
            };
            match (place, kind) {
                (Place::Query, _) => self.query.push(Item::Atom(table, args)),
                (_, TableKind::Function) => self.actions.push(Do::Act(Act::Lookup(table, args))),
                _ => self.actions.push(Do::Act(Act::Insert(table, args))),
            }
            return Ok(out);
        }
        let schema = self.schema;
        let mut overloads: Vec<ExternId> = (schema.primitives(name))
            .filter(|&prim| schema.primitive(prim).1.inputs.len() == args.len())
            .collect();
        if overloads.is_empty() {
            let message = format!(
                "no table or primitive `{name}` takes {} arguments",
                args.len()
            );
            return Err(self.type_error(span, message));
        }
        let context = match place {
            Place::Query => self.contexts.0,
            Place::Action | Place::Effect => self.contexts.1,
        };
        overloads.retain(|&prim| schema.primitive_contexts(prim).contains(&context));
        if overloads.is_empty() {
            let message = format!("`{name}` cannot run in a {context:?} context");
            return Err(self.type_error(span, message));
        }
        let out = self.var(format!("({name} ...)"), None, None);
        args.push(out);
        let call = self.calls.len();
        let name = name.to_owned();
        self.calls.push(Call {
            span,
            name,
            overloads,
            args,
        });
        match place {
            Place::Query => self.query.push(Item::Call(call)),
            Place::Action => self.actions.push(Do::Prim(call)),
            Place::Effect => self.actions.push(Do::Effect(call)),
        }
        Ok(out)
    }

    // `(find-or-make (T cells...) ...)`: the rows of one group, each
    // without its group column.
    fn find_or_make(&mut self, span: Span, rows: &[Expr], place: Place) -> Result<Var, Error> {
        let error = |typer: &Self, message: &str| Err(typer.type_error(span, message.to_owned()));
        if place == Place::Query {
            return error(self, "find-or-make is an action");
        }
        let (mut tables, mut args, mut first) = (Vec::new(), Vec::new(), None);
        for row in rows {
            let Expr::Call(_, name, cells) = row else {
                return error(self, "find-or-make takes rows of tables");
            };
            let table = self.schema.table_named(name);
            let group = table.and_then(|table| self.schema.db.group_at(table));
            let Some((table, group)) = table.zip(group) else {
                return error(self, &format!("`{name}` is no member table of a group"));
            };
            if *first.get_or_insert(group) != group {
                return error(self, "find-or-make writes the rows of one group");
            }
            if cells.len() + 1 != self.schema.table(table).columns().len() {
                return error(
                    self,
                    &format!("a row of `{name}` takes all but its group column"),
                );
            }
            tables.push(table);
            args.extend(self.terms(cells)?);
        }
        let Some(&first) = tables.first() else {
            return error(self, "find-or-make takes at least one row");
        };
        let sort = self.schema.table(first).columns()[0];
        let group = self.var(format!("({FIND_OR_MAKE} ...)"), None, Some(sort));
        args.push(group);
        self.actions.push(Do::Act(Act::FindOrMake(tables, args)));
        Ok(group)
    }

    // The table an action writes, with its key checked.
    fn written(&self, span: Span, name: &str, key: usize) -> Result<TableId, Error> {
        let table = self.schema.table_named(name);
        let table = table.ok_or_else(|| self.type_error(span, format!("no table `{name}`")))?;
        let arity = self.schema.table(table).key_arity();
        if key != arity {
            return Err(self.type_error(span, format!("`{name}` takes {arity} arguments")));
        }
        Ok(table)
    }

    fn terms(&mut self, exprs: &[Expr]) -> Result<Vec<Var>, Error> {
        exprs.iter().map(|e| self.term(e, Place::Action)).collect()
    }

    fn action(&mut self, action: &Action) -> Result<(), Error> {
        match action {
            Action::Let(span, name, expr) => {
                if self.bound.contains_key(name) {
                    return Err(self.type_error(*span, format!("`{name}` is bound already")));
                }
                let var = self.term(expr, Place::Action)?;
                self.bound.insert(name.clone(), var);
            }
            Action::Set(span, name, args, value) => {
                let table = self.written(*span, name, args.len())?;
                if self.schema.table(table).kind() == TableKind::Relation {
                    return Err(self.type_error(*span, format!("`{name}` has no output to set")));
                }
                let mut args = self.terms(args)?;
                args.push(self.term(value, Place::Action)?);
                self.actions.push(Do::Act(Act::Insert(table, args)));
            }
            Action::Change(span, change, name, args) => {
                let table = self.written(*span, name, args.len())?;
                let function = self.schema.table(table).kind() == TableKind::Function;
                if *change == Change::Subsume && function {
                    return Err(Error::SubsumeMergeError(name.clone(), *span));
                }
                let args = self.terms(args)?;
                self.actions.push(Do::Act(match change {
                    Change::Delete => Act::Delete(table, args),
                    Change::Subsume => Act::Subsume(table, args),
                }));
            }
            Action::Union(_, a, b) => {
                let (a, b) = (self.term(a, Place::Action)?, self.term(b, Place::Action)?);
                self.actions.push(Do::Act(Act::Union(a, b)));
            }
            Action::Panic(_, message) => self.actions.push(Do::Act(Act::Panic(message.clone()))),
            Action::Expr(_, expr) => _ = self.term(expr, Place::Effect)?,
        }
        Ok(())
    }

    // Returns whether the sort was unknown.
    fn fix(&mut self, var: Var, sort: SortId) -> Result<bool, Error> {
        let var = self.find(var);
        match self.sorts[var as usize] {
            None => {
                self.sorts[var as usize] = Some(sort);
                Ok(true)
            }
            Some(known) if known == sort => Ok(false),
            Some(known) => {
                let names = (
                    self.schema.sort(known).name(),
                    self.schema.sort(sort).name(),
                );
                let message = format!(
                    "`{}` has sorts {} and {}",
                    self.names[var as usize], names.0, names.1
                );
                Err(self.type_error(self.span, message))
            }
        }
    }

    fn infer(&mut self) -> Result<(), Error> {
        let mut uses: Vec<(TableId, Vec<Var>)> = Vec::new();
        let mut unions: Vec<(Var, Var)> = Vec::new();
        let mut grouped: Vec<(Var, SortId)> = Vec::new();
        for item in &self.query {
            if let Item::Atom(table, args) = item {
                uses.push((*table, args.clone()));
            }
        }
        for item in &self.actions {
            match item {
                Do::Act(
                    Act::Insert(table, args)
                    | Act::Lookup(table, args)
                    | Act::Delete(table, args)
                    | Act::Subsume(table, args),
                ) => uses.push((*table, args.clone())),
                Do::Act(Act::Union(a, b)) => unions.push((*a, *b)),
                Do::Act(Act::FindOrMake(tables, args)) => {
                    let columns = |&table| &self.schema.table(table).columns()[1..];
                    let sorts = tables.iter().flat_map(columns).copied();
                    grouped.extend(args.iter().copied().zip(sorts));
                }
                _ => {}
            }
        }
        for (table, args) in &uses {
            for (&var, &sort) in args.iter().zip(self.schema.table(*table).columns()) {
                self.fix(var, sort)?;
            }
        }
        for (var, sort) in grouped {
            self.fix(var, sort)?;
        }
        loop {
            let mut progress = false;
            for &(a, b) in &unions {
                let (sa, sb) = (self.sort(a), self.sort(b));
                if let Some(sort) = sa.or(sb) {
                    progress |= self.fix(a, sort)? | self.fix(b, sort)?;
                }
            }
            for c in 0..self.calls.len() {
                progress |= self.narrow(c)?;
            }
            if !progress {
                break;
            }
        }
        if let Some(call) = self.calls.iter().find(|call| call.overloads.len() > 1) {
            let message = format!("`{}` has several overloads that fit", call.name);
            return Err(self.type_error(call.span, message));
        }
        let unknown =
            (0..self.names.len() as Var).find(|&v| self.find(v) == v && self.sort(v).is_none());
        if let Some(var) = unknown {
            let message = format!("cannot infer the sort of `{}`", self.names[var as usize]);
            return Err(self.type_error(self.span, message));
        }
        if unions.iter().any(|&(a, _)| {
            self.sort(a)
                .is_some_and(|sort| !self.schema.sort(sort).kind().has_ids())
        }) {
            return Err(self.type_error(self.span, "cannot union base values".to_owned()));
        }
        Ok(())
    }

    fn sort(&self, var: Var) -> Option<SortId> {
        self.sorts[self.find(var) as usize]
    }

    // Drops the overloads of a call that disagree with known sorts, and
    // fixes the sorts of a call with one overload left.
    fn narrow(&mut self, call: usize) -> Result<bool, Error> {
        let schema = self.schema;
        let sorts: Vec<Option<SortId>> = self.calls[call]
            .args
            .iter()
            .map(|&v| self.sort(v))
            .collect();
        let fits = |prim: &ExternId| {
            let signature = schema.primitive(*prim).1;
            let wanted = signature.inputs.iter().chain([&signature.output]);
            wanted
                .zip(&sorts)
                .all(|(&want, have)| have.is_none_or(|have| have == want))
        };
        let before = self.calls[call].overloads.len();
        self.calls[call].overloads.retain(fits);
        let Call {
            span,
            name,
            overloads,
            args,
        } = &self.calls[call];
        let mut progress = overloads.len() < before;
        match overloads[..] {
            [] => {
                let message = format!("no overload of `{name}` fits the sorts");
                Err(self.type_error(*span, message))
            }
            [prim] => {
                let signature = schema.primitive(prim).1;
                let wanted: Vec<SortId> = signature
                    .inputs
                    .iter()
                    .chain([&signature.output])
                    .copied()
                    .collect();
                for (var, sort) in args.clone().into_iter().zip(wanted) {
                    progress |= self.fix(var, sort)?;
                }
                Ok(progress)
            }
            _ => Ok(progress),
        }
    }

    fn finish(self, rule: &ast::Rule) -> Rule {
        let find = |v: Var| self.find(v);
        let canonical = |args: &[Var]| -> Vec<Var> { args.iter().map(|&v| find(v)).collect() };
        let prim = |c: usize| (self.calls[c].overloads[0], canonical(&self.calls[c].args));
        let premise = (self.query.iter())
            .map(|item| match item {
                Item::Atom(table, args) => Atom {
                    target: Target::Table(*table),
                    args: canonical(args),
                },
                Item::Call(c) => {
                    let (prim, args) = prim(*c);
                    let target = Target::Prim(prim);
                    Atom { target, args }
                }
            })
            .collect();
        let actions = (self.actions.iter())
            .map(|item| match item {
                Do::Act(act) => act.clone(),
                Do::Prim(c) => {
                    let (prim, args) = prim(*c);
                    Act::Prim(prim, args)
                }
                Do::Effect(c) => {
                    let (prim, mut args) = prim(*c);
                    args.pop();
                    Act::Call(prim, args)
                }
            })
            .collect();
        let constants = (0..self.names.len() as Var)
            .map(|v| self.constants[find(v) as usize].clone())
            .collect();
        Rule {
            name: rule.name.clone(),
            naive: rule.eval_mode == RuleEvalMode::Naive,
            premise,
            actions,
            constants,
        }
    }
}
