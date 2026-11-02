// A typed rule as atoms over variables (thesis §3.2.2), and the single-rule
// optimizations of §3.3.1.

use super::permute::Symmetries;
use crate::ast::Literal;
use crate::sort::PROBE;
use crate::vm::value;
use crate::{Error, ExternId, PrimCtx, Schema, SortId, TableId, TableKind, Value};

pub(crate) type Var = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Target {
    Table(TableId),
    Prim(ExternId),
}

// A table atom holds every column, the output last. A primitive atom holds
// the inputs, then the output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Atom {
    pub(crate) target: Target,
    pub(crate) args: Vec<Var>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Act {
    // Writes a row. The output of a constructor row that nothing before binds
    // is the e-class of the key.
    Insert(TableId, Vec<Var>),
    // Binds the output of a key; a missing key fails the run.
    Lookup(TableId, Vec<Var>),
    // Binds the result of a primitive; no result drops the match.
    Prim(ExternId, Vec<Var>),
    // Calls a primitive for its effect.
    Call(ExternId, Vec<Var>),
    Union(Var, Var),
    // Binds the keyed group whose rows are exactly those of the tables, each
    // with the cells after its group column, in order in the variables
    // before the last; else makes one. The last variable is the group.
    FindOrMake(Vec<TableId>, Vec<Var>),
    Delete(TableId, Vec<Var>),
    Subsume(TableId, Vec<Var>),
    Panic(String),
}

// Actions run in order once the premise matched.
#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub(crate) name: String,
    pub(crate) naive: bool,
    pub(crate) premise: Vec<Atom>,
    pub(crate) actions: Vec<Act>,
    // The value of each variable that is a constant.
    pub(crate) constants: Vec<Option<Literal>>,
}

impl Atom {
    // The columns that determine the rest.
    pub(crate) fn keys(&self, schema: Schema<'_>) -> usize {
        match self.target {
            Target::Table(table) => schema.table(table).key_arity(),
            Target::Prim(_) => self.args.len() - 1,
        }
    }
}

impl Act {
    // Equal emitted rows do not imply equal effects for a custom merge or call.
    pub(crate) fn duplicate_safe(&self, schema: Schema<'_>) -> bool {
        match self {
            Self::Insert(table, _) => matches!(
                schema.table(*table).merge(),
                None | Some(
                    crate::Merge::AssertEq
                        | crate::Merge::Union
                        | crate::Merge::Old
                        | crate::Merge::New
                )
            ),
            Self::Union(..) => true,
            _ => false,
        }
    }

    fn args_mut(&mut self) -> &mut [Var] {
        match self {
            Self::Insert(_, args)
            | Self::Lookup(_, args)
            | Self::Prim(_, args)
            | Self::Call(_, args)
            | Self::Delete(_, args)
            | Self::Subsume(_, args)
            | Self::FindOrMake(_, args) => args,
            Self::Union(..) | Self::Panic(_) => &mut [],
        }
    }

    // The variable this action binds when nothing before does.
    fn binds(&self, schema: Schema<'_>) -> Option<(Var, bool)> {
        match self {
            Self::Insert(table, args) if schema.table(*table).kind() == TableKind::Constructor => {
                args.last().map(|&v| (v, true))
            }
            Self::Lookup(_, args) | Self::Prim(_, args) | Self::FindOrMake(_, args) => {
                args.last().map(|&v| (v, false))
            }
            _ => None,
        }
    }
}

impl Rule {
    // The variables the premise binds, constants included.
    pub(crate) fn premise_bound(&self) -> Vec<bool> {
        let mut bound: Vec<bool> = self.constants.iter().map(Option::is_some).collect();
        for atom in &self.premise {
            for &v in &atom.args {
                bound[v as usize] = true;
            }
        }
        bound
    }

    fn rename(&mut self, to: &mut [Var]) {
        let find = |to: &mut [Var], mut v: Var| {
            while to[v as usize] != v {
                v = to[v as usize];
            }
            v
        };
        for atom in &mut self.premise {
            atom.args.iter_mut().for_each(|v| *v = find(to, *v));
        }
        for act in &mut self.actions {
            if let Act::Union(a, b) = act {
                (*a, *b) = (find(to, *a), find(to, *b));
            }
            act.args_mut().iter_mut().for_each(|v| *v = find(to, *v));
        }
    }
}

// Merges variables and drops atoms until nothing changes: equal keys give
// equal outputs, duplicate atoms go, an action row the premise holds goes,
// and a union with a variable only an action makes merges the two.
pub(crate) fn simplify(rule: &mut Rule, schema: Schema<'_>, syms: &Symmetries) {
    let mut to: Vec<Var> = (0..rule.constants.len() as Var).collect();
    loop {
        rule.rename(&mut to);
        let (premise, action, merge) = if let Some((drop, merge)) = premise_edit(rule, schema, syms)
        {
            (drop, None, merge)
        } else if let Some((drop, merge)) = action_edit(rule, schema, syms) {
            (None, Some(drop), merge)
        } else {
            return;
        };
        if let Some(at) = premise {
            rule.premise.remove(at);
        }
        if let Some(at) = action {
            rule.actions.remove(at);
        }
        if let Some((from, into)) = merge {
            to[from as usize] = into;
        }
    }
}

type Edit<T> = Option<(T, Option<(Var, Var)>)>;

// An atom to drop, or two outputs to merge.
fn premise_edit(rule: &Rule, schema: Schema<'_>, syms: &Symmetries) -> Edit<Option<usize>> {
    let constant = |v: Var| rule.constants[v as usize].is_some();
    for (i, a) in rule.premise.iter().enumerate() {
        let keys = a.keys(schema);
        for (j, b) in rule.premise.iter().enumerate().skip(i + 1) {
            if a.target != b.target || !syms.same_key(a.target, &a.args, &b.args, keys) {
                continue;
            }
            if keys == a.args.len() || a.args[keys] == b.args[keys] {
                return Some((Some(j), None));
            }
            let (x, y) = (a.args[keys], b.args[keys]);
            match (constant(x), constant(y)) {
                (_, false) => return Some((None, Some((y, x)))),
                (false, true) => return Some((None, Some((x, y)))),
                (true, true) => {}
            }
        }
    }
    None
}

// An action to drop, with the variable it made merged into one bound already.
fn action_edit(rule: &Rule, schema: Schema<'_>, syms: &Symmetries) -> Edit<usize> {
    let bound = rule.premise_bound();
    let mut defined: Vec<Option<(usize, bool)>> = vec![None; rule.constants.len()];
    for (k, act) in rule.actions.iter().enumerate() {
        if let Some((v, entry)) = act.binds(schema)
            && !bound[v as usize]
            && defined[v as usize].is_none()
        {
            defined[v as usize] = Some((k, entry));
        }
    }
    let defined_at = |v: Var| defined[v as usize].map(|(k, _)| k);
    // Whether `x`, which an entry makes, can merge into `y`, bound before it.
    let merges = |x: Var, y: Var| {
        defined[x as usize].is_some_and(|(kx, entry)| {
            entry && (bound[y as usize] || defined_at(y).is_some_and(|ky| ky < kx))
        })
    };
    for (k, act) in rule.actions.iter().enumerate() {
        let (table, args, lookup) = match *act {
            Act::Insert(table, ref args) => (table, args, false),
            Act::Lookup(table, ref args) => (table, args, true),
            Act::Union(a, b) if a == b => return Some((k, None)),
            Act::Union(a, b) if merges(a, b) => return Some((k, Some((a, b)))),
            Act::Union(a, b) if merges(b, a) => return Some((k, Some((b, a)))),
            _ => continue,
        };
        if !lookup && !act.duplicate_safe(schema) {
            continue;
        }
        let keys = schema.table(table).key_arity();
        let target = Target::Table(table);
        // A function set must still be replayed if another rule deletes its row.
        let premise = rule.premise.iter().filter(|atom| {
            atom.target == target && (lookup || schema.table(table).kind() != TableKind::Function)
        });
        let earlier = rule.actions[..k]
            .iter()
            .rev()
            .take_while(|other| lookup || other.duplicate_safe(schema))
            .find_map(|other| match other {
                Act::Insert(t, args) if !lookup && *t == table => Some(args),
                Act::Lookup(t, args) if lookup && *t == table => Some(args),
                _ => None,
            });
        let same = premise
            .map(|atom| &atom.args)
            .chain(earlier)
            .find(|other| syms.same_key(target, other, args, keys));
        let Some(same) = same else { continue };
        if keys == args.len() || same[keys] == args[keys] {
            return Some((k, None));
        }
        if defined_at(args[keys]) == Some(k) {
            return Some((k, Some((args[keys], same[keys]))));
        }
    }
    None
}

// Runs the actions of a rule whose query only reads globals, at once through
// `ctx`, and returns the sort and value the probe saw, if one ran.
pub(crate) fn run_actions(
    rule: &Rule,
    ctx: &mut PrimCtx<'_>,
) -> Result<Option<(SortId, Value)>, Error> {
    let constants = rule.constants.iter();
    let mut vals: Vec<Option<Value>> = constants
        .map(|c| c.as_ref().map(|c| value(ctx.base_values(), c)))
        .collect();
    let get = |vals: &[Option<Value>], args: &[Var]| -> Vec<Value> {
        let bound = |&v: &Var| vals[v as usize].expect("an action reads bound variables");
        args.iter().map(bound).collect()
    };
    let missing = |ctx: &PrimCtx<'_>, table: TableId| Error::Missing {
        table: ctx.tables[table.index()].schema.name().to_owned(),
    };
    let failed = |ctx: &PrimCtx<'_>, prim: ExternId| {
        let name = &ctx.prims[prim.index()].name;
        Error::Panic(format!("`{name}` has no result"))
    };
    for atom in &rule.premise {
        let (Target::Table(table), Some((&out, key))) = (atom.target, atom.args.split_last())
        else {
            return Err(Error::Unsupported("a query outside a rule".to_owned()));
        };
        let found = ctx.lookup(table, &get(&vals, key));
        vals[out as usize] = Some(found.ok_or_else(|| missing(ctx, table))?);
    }
    let mut probed = None;
    for act in &rule.actions {
        match act {
            Act::Insert(table, args) => {
                let keys = ctx.tables[table.index()].schema.key_arity();
                match args.get(keys) {
                    Some(&out) if vals[out as usize].is_none() => {
                        vals[out as usize] = ctx.entry(*table, &get(&vals, &args[..keys]));
                    }
                    _ => ctx.insert(*table, &get(&vals, args)),
                }
            }
            Act::Lookup(table, args) => {
                let (&out, key) = args.split_last().expect("a lookup has an output");
                let found = ctx.entry(*table, &get(&vals, key));
                vals[out as usize] = Some(found.ok_or_else(|| missing(ctx, *table))?);
            }
            Act::Prim(prim, args) => {
                let (&out, inputs) = args.split_last().expect("a primitive has an output");
                let found = ctx.call(*prim, &get(&vals, inputs));
                vals[out as usize] = Some(found.ok_or_else(|| failed(ctx, *prim))?);
            }
            Act::Call(prim, args) if ctx.prims[prim.index()].name == PROBE => {
                let sort = ctx.prims[prim.index()].signature.inputs[0];
                probed = Some((sort, get(&vals, args)[0]));
            }
            Act::Call(prim, args) => {
                _ = ctx
                    .call(*prim, &get(&vals, args))
                    .ok_or_else(|| failed(ctx, *prim))?
            }
            Act::Union(a, b) => ctx.union(get(&vals, &[*a])[0], get(&vals, &[*b])[0]),
            Act::Delete(table, args) => ctx.remove(*table, &get(&vals, args)),
            Act::Subsume(table, args) => ctx.subsume(*table, &get(&vals, args)),
            Act::Panic(message) => return Err(Error::Panic(message.clone())),
            Act::FindOrMake(..) => {
                let message = "find-or-make outside a rule".to_owned();
                return Err(Error::Unsupported(message));
            }
        }
    }
    Ok(probed)
}
