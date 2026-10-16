//! The type check: the value types of a batch of ops joined with the scheme
//! rows of their names on the relational engine.
//!
//! A batch is loaded into one database. Every op, every type its values have
//! (and every type below those) and every other constructor argument is an
//! id. A `VALUE` row names an op's value by the scheme slot it fills, and a
//! term row names a type by its constructor and argument ids. A slot's
//! pattern is a conjunction over those rows, and a binder is one variable
//! wherever it occurs, so the join itself unifies: a value matches when its
//! type has the pattern's shape, and two values bind a binder alike exactly
//! when they bind it to one id.
//!
//! The context keeps the rows of each op name's scheme with its op-name
//! table: per slot the query for the values that do not fit it, per slot
//! reading a bounded binder the query for the values binding it to none of
//! its shapes, and per binder two slots name the query for the pairs of
//! values binding it differently. What a failing op's fitting values bind
//! is asked only to spell the message.

use std::collections::HashMap;
use std::sync::Arc;

use smallvec::{SmallVec, smallvec};
use tir_relational::query::{
    Atom, Calls, Guard, Host, Matches, Nested, Plan, PlanCache, Query, Resolution, Rows, Scalar,
    Select, Term,
};
use tir_relational::store::Table;
use tir_relational::{Cmp, Database, Expr, SpaceId, Var};
use tir_symbolic::scheme::{Arity, Scheme, SchemeSyntax, Slot};

use crate::{Context, Error, OpId, OpSchema, Param, Terms, TypeArg, TypeId, ValueId};

const SPACE: SpaceId = SpaceId::nth(0);

// The tables, keyed `[plain cell, ids..] -> value`. A row atom selects its
// rows by the plain cell.
// `[slot, op] -> type`, a bag; tag = the value's position among the op's
// operands, then its results.
const VALUE: u32 = 0;
// `[type number] -> type`.
const TYPE: u32 = 1;
// `[n] -> nat`, for a nat below 2^32.
const NAT: u32 = 2;
// `[ctor, args..] -> type` for a term of `n` arguments, at `TERM + n - 1`.
// A type that leaves its constructor's trailing optional argument out has a
// row with the batch's absent id in its place too, so a pattern variable
// there binds the absence.
const TERM: u32 = 3;
const MAX_ARGS: usize = 4;

pub(crate) struct SchemeRows {
    pub(crate) scheme: Arc<Scheme>,
    schema: &'static OpSchema,
    first_result: usize,
    misfits: Vec<Check>,
    clashes: Vec<Check>,
}

struct Check {
    plan: Plan,
    selects: Vec<(u32, u32)>,
    vars: Vec<Option<Var>>,
}

impl SchemeRows {
    pub(crate) fn new(terms: &Terms, schema: &'static OpSchema) -> Result<Self, String> {
        let scheme = SchemeSyntax::parse(schema.scheme)?.resolve(terms)?;
        let names = scheme.names().len();
        let slots: Vec<_> = scheme.slots().collect();
        let mut misfits = Vec::new();
        for (slot, def) in slots.iter().enumerate() {
            let mut misfit = Builder::new(names);
            let (value, atoms) = misfit.value(terms, slot, &def.pattern, 0)?;
            if !atoms.is_empty() {
                let shape = Nested {
                    atoms,
                    guards: Vec::new(),
                };
                misfits.push(misfit.check(vec![value], Vec::new(), vec![shape]));
            }
        }
        let mut clashes = Vec::new();
        let binders = scheme.names().iter().enumerate();
        for (binder, _) in binders.filter(|(_, name)| *name != "_") {
            let binder = binder as u32;
            let naming: Vec<usize> = (0..slots.len())
                .filter(|&slot| reads(terms, &slots[slot].pattern, binder))
                .collect();
            let shapes = scheme.bound(binder);
            if !shapes.is_empty() {
                for &slot in &naming {
                    misfits.push(unbounded(terms, names, binder, shapes, slot, &slots)?);
                }
            }
            for (at, &first) in naming.iter().enumerate() {
                let many = slots[first].arity == Arity::Many;
                let seconds = naming[at..]
                    .iter()
                    .filter(|&&second| second != first || many);
                for &second in seconds {
                    clashes.push(clash(terms, names, binder, [first, second], &slots)?);
                }
            }
        }
        Ok(Self {
            first_result: scheme.operands.len(),
            scheme: Arc::new(scheme),
            schema,
            misfits,
            clashes,
        })
    }
}

// The values of `slot` binding `binder` to a type of none of `shapes`.
fn unbounded(
    terms: &Terms,
    names: usize,
    binder: u32,
    shapes: &[TypeArg],
    slot: usize,
    slots: &[&Slot],
) -> Result<Check, String> {
    let mut query = Builder::new(names);
    let (value, mut atoms) = query.value(terms, slot, &slots[slot].pattern, 0)?;
    atoms.insert(0, value);
    let bound = query.vars[binder as usize].expect("the slot reads the binder");
    let mut nots = Vec::new();
    for shape in shapes {
        let mut atoms = Vec::new();
        query.pattern(terms, shape, Some(bound), &mut atoms)?;
        let guards = Vec::new();
        nots.push(Nested { atoms, guards });
    }
    Ok(query.check(atoms, Vec::new(), nots))
}

// Per slot, what the values that fit it bind.
fn fits(terms: &Terms, scheme: &Scheme) -> Result<Vec<Check>, String> {
    let names = scheme.names().len();
    let slots = scheme.slots().enumerate();
    let fit = |(slot, def): (usize, &Slot)| {
        let mut fit = Builder::new(names);
        let (value, mut atoms) = fit.value(terms, slot, &def.pattern, 0)?;
        atoms.insert(0, value);
        Ok(fit.check(atoms, Vec::new(), Vec::new()))
    };
    slots.map(fit).collect()
}

// The pairs of values of slots `first` and `second` that bind `binder` to
// different ids.
fn clash(
    terms: &Terms,
    names: usize,
    binder: u32,
    [first, second]: [usize; 2],
    slots: &[&Slot],
) -> Result<Check, String> {
    let mut query = Builder::new(names);
    let (value, mut atoms) = query.value(terms, first, &slots[first].pattern, 0)?;
    atoms.insert(0, value);
    let bound = query.vars[binder as usize].expect("the slot reads the binder");
    query.vars.fill(None);
    let (value, pattern) = query.value(terms, second, &slots[second].pattern, 1)?;
    atoms.push(value);
    atoms.extend(pattern);
    let other = query.vars[binder as usize].expect("the slot reads the binder");
    let mut guards = vec![Guard::Distinct(smallvec![(bound, other)])];
    if first == second {
        guards.push(Guard::Cmp(Cmp::Lt, Expr::Scalar(0), Expr::Scalar(1)));
    }
    Ok(query.check(atoms, guards, Vec::new()))
}

// Whether `pattern` reads scheme variable `var`.
fn reads(terms: &Terms, pattern: &TypeArg, var: u32) -> bool {
    match pattern {
        TypeArg::Var(read) => *read == var,
        TypeArg::Type(id) => terms
            .term(*id)
            .args()
            .iter()
            .any(|arg| reads(terms, arg, var)),
        _ => false,
    }
}

fn has_vars(terms: &Terms, pattern: &TypeArg) -> bool {
    match pattern {
        TypeArg::Var(_) => true,
        TypeArg::Type(id) => terms
            .term(*id)
            .args()
            .iter()
            .any(|arg| has_vars(terms, arg)),
        _ => false,
    }
}

// Builds one query: the op is variable 0, the root.
struct Builder {
    next: u32,
    scalars: u32,
    selects: Vec<(u32, u32)>,
    vars: Vec<Option<Var>>,
}

impl Builder {
    fn new(names: usize) -> Self {
        Self {
            next: 1,
            scalars: 0,
            selects: Vec::new(),
            vars: vec![None; names],
        }
    }

    fn var(&mut self) -> Var {
        self.next += 1;
        self.next - 1
    }

    fn rows(
        &mut self,
        table: u32,
        cell: u32,
        args: &[Var],
        value: Var,
        tag: Option<Scalar>,
    ) -> Atom {
        let param = self.selects.len() as u32;
        self.selects.push((table, cell));
        let mut terms: SmallVec<[Term; 6]> = smallvec![Term::Any];
        terms.extend(args.iter().map(|&arg| Term::Var(arg)));
        terms.push(Term::Var(value));
        Atom::Rows(Rows {
            select: Select { column: 0, param },
            terms,
            tag,
            cell: None,
            swap: None,
            tag_within: None,
        })
    }

    // The op's values of `slot`, their positions in `position`, and the atoms
    // holding their types to `pattern`.
    fn value(
        &mut self,
        terms: &Terms,
        slot: usize,
        pattern: &TypeArg,
        position: Scalar,
    ) -> Result<(Atom, Vec<Atom>), String> {
        let ty = self.var();
        self.scalars = self.scalars.max(position + 1);
        let value = self.rows(VALUE, slot as u32, &[0], ty, Some(position));
        let mut atoms = Vec::new();
        self.pattern(terms, pattern, Some(ty), &mut atoms)?;
        Ok((value, atoms))
    }

    // The variable `pattern` is bound to, `at` when given, with the atoms
    // that hold it to the pattern's shape.
    fn pattern(
        &mut self,
        terms: &Terms,
        pattern: &TypeArg,
        at: Option<Var>,
        atoms: &mut Vec<Atom>,
    ) -> Result<Var, String> {
        if let TypeArg::Var(scheme_var) = pattern {
            let bound = self.vars[*scheme_var as usize];
            let var = bound.or(at).unwrap_or_else(|| self.var());
            self.vars[*scheme_var as usize] = Some(var);
            return Ok(var);
        }
        let mut args: SmallVec<[Var; 4]> = SmallVec::new();
        let (table, cell) = match pattern {
            TypeArg::Nat(n) => {
                let n = u32::try_from(*n).map_err(|_| format!("nat {n} in a scheme"))?;
                (NAT, n)
            }
            TypeArg::Type(id) if !has_vars(terms, pattern) => (TYPE, id.number()),
            TypeArg::Type(id) => {
                let term = terms.term(*id);
                if term.args().len() > MAX_ARGS {
                    return Err(format!(
                        "a scheme pattern of more than {MAX_ARGS} arguments"
                    ));
                }
                for arg in term.args() {
                    args.push(self.pattern(terms, arg, None, atoms)?);
                }
                (TERM + args.len() as u32 - 1, term.ctor().number())
            }
            other => return Err(format!("{other:?} in a scheme pattern")),
        };
        let var = at.unwrap_or_else(|| self.var());
        let atom = self.rows(table, cell, &args, var, None);
        atoms.push(atom);
        Ok(var)
    }

    fn check(self, atoms: Vec<Atom>, guards: Vec<Guard>, nots: Vec<Nested>) -> Check {
        let plan = Plan::compile(Query {
            space: SPACE,
            vars: self.next,
            scalars: self.scalars,
            params: self.selects.len() as u32,
            root: 0,
            atoms,
            guards,
            nots,
        });
        Check {
            plan,
            selects: self.selects,
            vars: self.vars,
        }
    }
}

impl Check {
    fn search(&self, db: &Database<(), 1>, roots: &[u32]) -> Matches {
        let selects = Selects(&self.selects);
        self.plan.search_roots(
            db,
            &selects,
            roots.iter().copied(),
            None::<&fn(Var, u32) -> bool>,
            (false, 0),
            &mut PlanCache::default(),
        )
    }
}

struct Selects<'a>(&'a [(u32, u32)]);

impl Calls for Selects<'_> {
    fn call(&self, _: u32, _: &[u64], _: &mut [u64]) -> bool {
        false
    }
}

impl Host for Selects<'_> {
    fn version(&self) -> u64 {
        1
    }

    fn resolve(&self, param: u32, since: Option<u64>, out: &mut Resolution) {
        if since.is_none() {
            let (table, cell) = self.0[param as usize];
            out.tables.push((table, smallvec![cell]));
        }
    }
}

/// The scheme `schema` resolves to in the context's term store.
pub fn scheme_of(context: &Context, schema: &'static OpSchema) -> Result<Arc<Scheme>, String> {
    let name = context.intern_op_name(schema.dialect, schema.name);
    match context.scheme_rows(name) {
        Some(rows) => Ok(rows?.scheme.clone()),
        None => Err(format!(
            "{}.{} declares no scheme",
            schema.dialect, schema.name
        )),
    }
}

// An op of the batch: its rows and its values with their scheme slots.
struct Checked {
    op: OpId,
    rows: Arc<SchemeRows>,
    values: Vec<(usize, ValueId)>,
}

/// Check the value types of `ops` against their schemes in one join of their
/// value types with the scheme rows of their names. Ops no `operation!`
/// declared are not checked.
pub fn check_types(context: &Context, ops: &[OpId]) -> Result<(), Error> {
    let mut checked = Vec::new();
    for &op in ops {
        let Some(rows) = context.scheme_rows(context.op_name_id(op)) else {
            continue;
        };
        let instance = context.get_op(op);
        let name = instance.name();
        let name = name.as_str();
        let rows = rows.map_err(|error| Error::VerificationError(format!("{name}: {error}")))?;
        let operands = crate::operation::operand_fields(&instance, name, rows.schema.operands)?;
        let results = crate::operation::result_fields(&instance, name, rows.schema.results)?;
        let slots = operands
            .into_iter()
            .chain(results.into_iter().map(|slot| slot + rows.first_result));
        let (operands, results) = (instance.operands(), instance.results());
        let values = slots.zip(operands.iter().chain(results.iter()).copied());
        let values = values.collect();
        checked.push(Checked { op, rows, values });
    }
    let mut batch = Batch::new();
    for _ in &checked {
        batch.mint(None);
    }
    batch.absent = batch.mint(None);
    let terms = context.terms();
    for (root, op) in checked.iter().enumerate() {
        for (position, &(slot, value)) in op.values.iter().enumerate() {
            let ty = batch.load(terms, context.get_value(value).ty());
            let key = [slot as u32, root as u32];
            batch.db.insert(VALUE, &key, &[], (ty, 0), position as u32);
        }
    }
    // Each op name's queries run once, from every op of that name.
    let mut roots: HashMap<*const SchemeRows, (Arc<SchemeRows>, Vec<u32>)> = HashMap::new();
    for (root, op) in checked.iter().enumerate() {
        let entry = roots.entry(Arc::as_ptr(&op.rows));
        let (_, ids) = entry.or_insert_with(|| (op.rows.clone(), Vec::new()));
        ids.push(root as u32);
    }
    // The first op of the batch that fails, at the first value that fails.
    let mut failed: Option<(u32, usize)> = None;
    for (rows, ids) in roots.values() {
        let checks = rows.misfits.iter().chain(&rows.clashes);
        for check in checks {
            let matches = check.search(&batch.db, ids);
            for index in 0..matches.len() {
                let (root, _, scalars) = matches.get(index);
                let position = scalars.iter().copied().max().expect("a position") as usize;
                if failed.is_none_or(|first| (root, position) < first) {
                    failed = Some((root, position));
                }
            }
        }
    }
    match failed {
        Some((root, position)) => Err(mismatch(
            context,
            &batch,
            &checked[root as usize],
            root,
            position,
        )),
        None => Ok(()),
    }
}

struct Batch {
    db: Database<(), 1>,
    named: Vec<Option<TypeArg>>,
    types: HashMap<TypeId, u32>,
    leaves: HashMap<TypeArg, u32>,
    absent: u32,
}

impl Batch {
    fn new() -> Self {
        let mut db = Database::new();
        db.add_table(Table::bag(2).plain(1), SPACE);
        for _ in [TYPE, NAT] {
            db.add_table(Table::new(1).plain(1), SPACE);
        }
        for args in 1..=MAX_ARGS {
            db.add_table(Table::new(args + 1).plain(1), SPACE);
        }
        Self {
            db,
            named: Vec::new(),
            types: HashMap::new(),
            leaves: HashMap::new(),
            absent: u32::MAX,
        }
    }

    fn mint(&mut self, named: Option<TypeArg>) -> u32 {
        let id = self.db.mint(SPACE, 0, false);
        self.named.push(named);
        id
    }

    // The id of `ty`, with its rows and its arguments'.
    fn load(&mut self, terms: &Terms, ty: TypeId) -> u32 {
        if let Some(&id) = self.types.get(&ty) {
            return id;
        }
        let term = terms.term(ty);
        let mut args: SmallVec<[u32; 4]> =
            term.args().iter().map(|arg| self.arg(terms, arg)).collect();
        let id = self.mint(Some(TypeArg::Type(ty)));
        self.types.insert(ty, id);
        self.db.insert(TYPE, &[ty.number()], &[], (id, 0), 0);
        let params = terms.ctor_info(term.ctor()).params;
        let mut row = |args: &[u32]| {
            if (1..=MAX_ARGS).contains(&args.len()) {
                let mut key: SmallVec<[u32; 5]> = smallvec![term.ctor().number()];
                key.extend(args.iter().copied());
                let table = TERM + args.len() as u32 - 1;
                self.db.insert(table, &key, &[], (id, 0), 0);
            }
        };
        row(&args);
        if args.len() + 1 == params.len() && matches!(params.last(), Some((_, Param::Opt(_)))) {
            args.push(self.absent);
            row(&args);
        }
        id
    }

    fn arg(&mut self, terms: &Terms, arg: &TypeArg) -> u32 {
        if let TypeArg::Type(ty) = arg {
            return self.load(terms, *ty);
        }
        if let Some(&id) = self.leaves.get(arg) {
            return id;
        }
        let id = self.mint(Some(arg.clone()));
        self.leaves.insert(arg.clone(), id);
        if let TypeArg::Nat(n) = arg
            && let Ok(n) = u32::try_from(*n)
        {
            self.db.insert(NAT, &[n], &[], (id, 0), 0);
        }
        id
    }
}

fn mismatch(context: &Context, batch: &Batch, op: &Checked, root: u32, position: usize) -> Error {
    let scheme = &op.rows.scheme;
    let instance = context.get_op(op.op);
    let operands = instance.operands().len();
    let name = instance.name();
    let at = |value: usize| match value < operands {
        true => format!("operand {}", value + 1),
        false => format!("result {}", value - operands + 1),
    };
    // Per binder, the first value binding it and what to.
    let names = scheme.names();
    let terms = context.terms();
    let fits = fits(terms, scheme).expect("the checks of the scheme were built");
    let mut bound: HashMap<u32, (usize, TypeArg)> = HashMap::new();
    for fits in &fits {
        let matches = fits.search(&batch.db, &[root]);
        for index in 0..matches.len() {
            let (_, bindings, scalars) = matches.get(index);
            let from = scalars[0] as usize;
            if from >= position {
                continue;
            }
            for (var, query) in fits.vars.iter().enumerate() {
                let Some(query) = query.filter(|_| names[var] != "_") else {
                    continue;
                };
                // A binder bound to an absent argument is left as written.
                let named = &batch.named[bindings[query as usize].id as usize];
                let Some(value) = named.clone() else {
                    continue;
                };
                let first = bound.entry(var as u32).or_insert((from, value.clone()));
                if from < first.0 {
                    *first = (from, value);
                }
            }
        }
    }
    let (slot, value) = op.values[position];
    let pattern = &scheme.slots().nth(slot).expect("a slot").pattern;
    let show = |arg: &TypeArg| context.type_arg_to_string(arg, names);
    let located = context
        .op_location(op.op)
        .map(|span| format!(" at byte {}", span.0))
        .unwrap_or_default();
    let mut message = format!(
        "{}{located}: {} is {}, expected {}",
        name.as_str(),
        at(position),
        context.type_to_string(context.get_value(value).ty()),
        show(&substitute(terms, pattern, &bound)),
    );
    let mut read = Vec::new();
    collect_vars(terms, pattern, &mut read);
    let binders: Vec<u32> = read
        .into_iter()
        .filter(|&var| names[var as usize] != "_")
        .collect();
    let bindings = binders.iter().filter(|var| bound.contains_key(var));
    for (index, var) in bindings.enumerate() {
        let (from, value) = &bound[var];
        message += if index == 0 { "; " } else { ", " };
        message += &format!(
            "{} := {} from {}",
            names[*var as usize],
            show(value),
            at(*from)
        );
    }
    for &var in &binders {
        let shapes: Vec<String> = scheme.bound(var).iter().map(show).collect();
        if !shapes.is_empty() {
            message += &format!("; {} is one of {}", names[var as usize], shapes.join(" | "));
        }
    }
    Error::VerificationError(message)
}

fn substitute(terms: &Terms, arg: &TypeArg, bound: &HashMap<u32, (usize, TypeArg)>) -> TypeArg {
    match arg {
        TypeArg::Var(var) => bound
            .get(var)
            .map_or_else(|| arg.clone(), |(_, value)| value.clone()),
        TypeArg::Type(id) if has_vars(terms, arg) => {
            let term = terms.term(*id);
            let args = term
                .args()
                .iter()
                .map(|arg| substitute(terms, arg, bound))
                .collect();
            let id = terms
                .intern(term.ctor(), args)
                .expect("a pattern with its binders bound");
            TypeArg::Type(id)
        }
        _ => arg.clone(),
    }
}

fn collect_vars(terms: &Terms, arg: &TypeArg, vars: &mut Vec<u32>) {
    match arg {
        TypeArg::Var(var) if !vars.contains(var) => vars.push(*var),
        TypeArg::Type(id) => {
            for arg in terms.term(*id).args() {
                collect_vars(terms, arg, vars);
            }
        }
        _ => {}
    }
}
