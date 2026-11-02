// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/lib.rs, src/typechecking.rs and src/command_macro.rs. See libs/egglog/LICENSE-MIT.

//! egglog on a columnar database: tables of rows over sorts, one union-find
//! over every Eq sort, and a rebuild that keeps rows canonical. Rules compile
//! into a [`Program`] that a batched interpreter runs, or that loads from
//! bytes with no planning. [`EGraph`] is the egglog interface on top.

/// The conversions between Rust values and rows.
pub mod api {
    pub use crate::exec_state::{
        ApiError, FromValue, FromValues, IntoValue, IntoValues, RawValues,
    };
}
pub mod ast;
mod core;
mod desugar;
mod exec_state;
pub mod extract;
#[cfg(feature = "parser")]
mod grammar;
mod plan;
pub mod prelude;
mod report;
mod rules;
pub mod scheduler;
pub mod sort;
mod termdag;
mod typecheck;
mod vm;

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug, Display, Formatter};
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tir_adt::FxBuildHasher;

pub use ast::Schedule;
use ast::{
    Action, Command, Expr, Fact, Facts, Literal, PrintFunctionMode, Rule, RuleEvalMode, Span,
};
pub use core::*;
pub use exec_state::{
    ApiError, Context, Core, Enode, FromValue, FromValues, FullState, FunctionEntry, IntoValue,
    IntoValues, PureState, RawValues, Read, ReadState, Write, WriteState,
};
use extract::DefaultCost;
pub use extract::Extractor;
use prelude::BaseSort;
pub use report::{IterationReport, ReportLevel, RuleReport, RuleSetReport, RunReport};
pub use rules::{Mode, Rules};
use scheduler::SchedulerRecord;
pub use sort::Sort;
use sort::{EqSort, MkSort, PROBE, Presort};
pub use termdag::{OrdTerm, Term, TermDag, TermId};
pub use vm::{LoadError, Program};

pub const GLOBAL_NAME_PREFIX: &str = "$";

pub type ArcSort = Arc<dyn Sort>;

type Map<K, V> = HashMap<K, V, FxBuildHasher>;
// Rows a collector primitive saw: how many calls, and their arguments.
type Sink = Arc<Mutex<(usize, Vec<Value>)>>;

/// A primitive: a name and the sorts it takes, then the sort it returns.
pub trait Primitive: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn signature(&self) -> Vec<ArcSort>;
}

macro_rules! prim_traits {
    ($($(#[$doc:meta])* $trait:ident: $state:ident, $add:ident;)+) => {
        $($(#[$doc])*
        pub trait $trait: Primitive {
            fn apply<'a, 'db>(&self, state: $state<'a, 'db>, args: &[Value]) -> Option<Value>;
        })+

        impl EGraph {
            $(/// Registers a primitive whose body gets this state.
            pub fn $add<T: $trait + Clone>(&mut self, x: T) {
                let signature = x.signature();
                let name = x.name().to_owned();
                let contexts = $state::valid_contexts();
                self.add_signed(&name, &signature, contexts, move |ctx, args| x.apply($state::new(ctx), args));
            })+
        }
    };
}

prim_traits! {
    /// A primitive that reads and writes nothing.
    PurePrim: PureState, add_pure_primitive;
    /// A primitive that writes rows, such as the action of a rule.
    WritePrim: WriteState, add_write_primitive;
    /// A primitive that reads rows.
    ReadPrim: ReadState, add_read_primitive;
    /// A primitive that reads and writes rows.
    FullPrim: FullState, add_full_primitive;
}

/// What a user-defined command returns.
pub trait UserDefinedCommandOutput: Debug + Display + Send + Sync {
    fn as_any(&self) -> &dyn Any
    where
        Self: 'static;
}

impl<T: Debug + Display + Send + Sync> UserDefinedCommandOutput for T {
    fn as_any(&self) -> &dyn Any
    where
        Self: 'static,
    {
        self
    }
}

/// A command with exclusive access to the e-graph, run as `(name args...)`.
pub trait UserDefinedCommand: Send + Sync {
    fn update(&self, egraph: &mut EGraph, args: &[Expr]) -> Result<Vec<CommandOutput>, Error>;
}

/// A rewrite of commands before they run, such as a library's own syntax.
pub trait CommandMacro: Send + Sync {
    /// The commands `command` stands for; `vec![command]` if it does not apply.
    fn transform(&self, command: Command, db: &Database) -> Result<Vec<Command>, Error>;
}

/// Command macros, applied in the order they were registered.
#[derive(Clone, Default)]
pub struct CommandMacroRegistry {
    macros: Vec<Arc<dyn CommandMacro>>,
}

impl CommandMacroRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, macro_impl: Arc<dyn CommandMacro>) {
        self.macros.push(macro_impl);
    }

    pub fn apply(&self, command: Command, db: &Database) -> Result<Vec<Command>, Error> {
        let mut commands = vec![command];
        for macro_impl in &self.macros {
            let each = commands
                .into_iter()
                .map(|command| macro_impl.transform(command, db));
            commands = each.collect::<Result<Vec<_>, _>>()?.concat();
        }
        Ok(commands)
    }
}

/// What a command printed.
#[derive(Clone, Debug)]
pub enum CommandOutput {
    PrintFunctionSize(usize),
    /// The name and size of every function.
    PrintAllFunctionsSize(Vec<(String, usize)>),
    ExtractBest(TermDag, DefaultCost, TermId),
    ExtractVariants(TermDag, Vec<TermId>),
    OverallStatistics(RunReport),
    /// A function and its rows as key terms and output terms.
    PrintFunction(Function, TermDag, Vec<(TermId, TermId)>, PrintFunctionMode),
    RunSchedule(RunReport),
    UserDefined(Arc<dyn UserDefinedCommandOutput>),
}

// A CSV field, quoted when it holds a separator, quote or line break.
fn csv(field: String) -> String {
    if field.contains([',', '"', '\n']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field
    }
}

impl Display for CommandOutput {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrintFunctionSize(size) => writeln!(f, "{size}"),
            Self::PrintAllFunctionsSize(sizes) => {
                let sizes: Vec<String> = sizes
                    .iter()
                    .map(|(name, size)| format!("({name} {size})"))
                    .collect();
                writeln!(f, "({})", sizes.join("\n "))
            }
            Self::ExtractBest(termdag, _, term) => writeln!(f, "{}", termdag.to_string(*term)),
            Self::ExtractVariants(termdag, terms) => {
                writeln!(f, "(")?;
                terms
                    .iter()
                    .try_for_each(|&term| writeln!(f, "   {}", termdag.to_string(term)))?;
                writeln!(f, ")")
            }
            Self::OverallStatistics(report) => write!(f, "Overall statistics:\n{report}"),
            Self::PrintFunction(function, termdag, rows, mode) => {
                let unit = function.func_type.output.name() == "Unit";
                let csv_mode = *mode == PrintFunctionMode::CSV;
                if !csv_mode {
                    writeln!(f, "(")?;
                }
                for &(term, output) in rows {
                    let output = (!unit).then(|| termdag.to_string(output));
                    if csv_mode {
                        let Term::App(name, children) = termdag.get(term) else {
                            unreachable!("a row prints as an application")
                        };
                        let fields = std::iter::once(name.clone())
                            .chain(children.iter().map(|&c| termdag.to_string(c)));
                        let fields: Vec<String> = fields.chain(output).map(csv).collect();
                        writeln!(f, "{}", fields.join(","))?;
                    } else {
                        let output = output
                            .map(|output| format!(" -> {output}"))
                            .unwrap_or_default();
                        writeln!(f, "   {}{output}", termdag.to_string(term))?;
                    }
                }
                if !csv_mode {
                    writeln!(f, ")")?;
                }
                Ok(())
            }
            Self::RunSchedule(_) => Ok(()),
            Self::UserDefined(output) => write!(f, "{output}"),
        }
    }
}

/// Whether a table makes e-classes or maps keys to values; a relation is a
/// constructor of `()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FunctionSubtype {
    Constructor,
    Custom,
}

impl FunctionSubtype {
    pub(crate) fn of(kind: TableKind) -> Self {
        match kind {
            TableKind::Function => Self::Custom,
            TableKind::Constructor | TableKind::Relation => Self::Constructor,
        }
    }
}

/// The signature of a table.
#[derive(Clone, Debug)]
pub struct FuncType {
    pub name: String,
    pub subtype: FunctionSubtype,
    pub input: Vec<ArcSort>,
    pub output: ArcSort,
}

impl FuncType {
    pub(crate) fn of(schema: &TableSchema, sorts: &[SortSchema]) -> Self {
        let sort = |id: SortId| sorts[id.index()].arcsort().clone();
        let unit = || {
            sorts
                .iter()
                .find(|s| s.name() == "Unit")
                .expect("the Unit sort")
                .arcsort()
                .clone()
        };
        Self {
            name: schema.name().to_owned(),
            subtype: FunctionSubtype::of(schema.kind()),
            input: schema.columns()[..schema.key_arity()]
                .iter()
                .map(|&s| sort(s))
                .collect(),
            output: schema
                .columns()
                .get(schema.key_arity())
                .map_or_else(unit, |&s| sort(s)),
        }
    }
}

/// A table as the e-graph declared it.
#[derive(Clone, Debug)]
pub struct Function {
    func_type: FuncType,
    let_binding: bool,
    cost: Option<DefaultCost>,
    unextractable: bool,
}

impl Function {
    pub fn name(&self) -> &str {
        &self.func_type.name
    }

    pub fn func_type(&self) -> &FuncType {
        &self.func_type
    }

    pub fn can_subsume(&self) -> bool {
        self.func_type.subtype == FunctionSubtype::Constructor
    }

    /// Whether a top-level `let` made it.
    pub fn is_let_binding(&self) -> bool {
        self.let_binding
    }

    /// Whether the e-graph made it for its own use.
    pub fn is_hidden(&self) -> bool {
        self.func_type.name.starts_with('@')
    }

    pub fn cost(&self) -> Option<DefaultCost> {
        self.cost
    }

    pub fn is_unextractable(&self) -> bool {
        self.unextractable
    }
}

// The names of sorts, tables, rulesets and globals, which no later
// declaration or variable may reuse; a global `$x` also claims `x`.
#[derive(Clone, Default)]
struct Names {
    seen: Map<String, Span>,
    aliases: Map<String, (String, Span)>,
}

impl Names {
    fn claim(&mut self, name: &str, span: Span) -> Result<(), Error> {
        if let Some(&old) = self.seen.get(name) {
            return Err(Error::Shadowing(name.to_owned(), old, span));
        }
        self.seen.insert(name.to_owned(), span);
        Ok(())
    }

    fn claim_global(&mut self, name: &str, span: Span) -> Result<(), Error> {
        self.claim(name, span)?;
        if let Some(alias) = name.strip_prefix(GLOBAL_NAME_PREFIX) {
            self.aliases
                .insert(alias.to_owned(), (name.to_owned(), span));
        }
        Ok(())
    }

    fn claim_variable(&mut self, name: &str, span: Span) -> Result<(), Error> {
        let alias = name.strip_prefix(GLOBAL_NAME_PREFIX).unwrap_or(name);
        if let Some((global, old)) = self.aliases.get(alias) {
            let message = format!("pattern variable `{name}` conflicts with global `{global}`");
            return Err(Error::Shadowing(message, *old, span));
        }
        self.claim(name, span)
    }
}

trait Extension: Any {
    fn clone_box(&self) -> Box<dyn Extension>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any + Clone> Extension for T {
    fn clone_box(&self) -> Box<dyn Extension> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl Clone for Box<dyn Extension> {
    fn clone(&self) -> Self {
        (**self).clone_box()
    }
}

/// An e-graph: a [`Database`], the rules on it, the program planned from
/// them, and the state of egglog commands.
#[derive(Clone)]
pub struct EGraph {
    pub(crate) db: Database,
    pub(crate) rules: Rules,
    pub(crate) program: Program,
    // The rule count the program was planned at, and whether it was loaded.
    planned: usize,
    loaded: bool,
    globals: HashSet<String, FxBuildHasher>,
    names: Names,
    // The sorts that have the generic primitives.
    sorts_done: usize,
    presorts: Map<String, MkSort>,
    pub(crate) collectors: Map<Vec<SortId>, Sink>,
    pub(crate) schedulers: Vec<Option<SchedulerRecord>>,
    commands: Map<String, Arc<dyn UserDefinedCommand>>,
    command_macros: CommandMacroRegistry,
    extension_state: Map<TypeId, Box<dyn Extension>>,
    rust_rules: Map<String, Arc<PrimFn>>,
    overall_run_report: RunReport,
    pub(crate) report_level: ReportLevel,
    fresh: usize,
    strict_mode: bool,
    reserved_symbols: bool,
    /// When false, every run of a rule matches the whole database.
    pub seminaive: bool,
}

impl Default for EGraph {
    fn default() -> Self {
        Self::from_database(Database::new())
    }
}

fn add_presort<S: Presort>(presorts: &mut Map<String, MkSort>) {
    presorts.insert(S::presort_name().to_owned(), S::make_sort);
}

impl EGraph {
    /// An e-graph over `db`, with the sorts and primitives of
    /// [`EGraph::default`] added to those it has.
    pub fn from_database(db: Database) -> Self {
        let program = Rules::new().compile(&db).expect("no rules to plan");
        let mut presorts = Map::default();
        add_presort::<sort::MapSort>(&mut presorts);
        add_presort::<sort::SetSort>(&mut presorts);
        add_presort::<sort::VecSort>(&mut presorts);
        add_presort::<sort::FunctionSort>(&mut presorts);
        add_presort::<sort::MultiSetSort>(&mut presorts);
        add_presort::<sort::PairSort>(&mut presorts);
        let mut eg = Self {
            db,
            rules: Rules::new(),
            program,
            planned: usize::MAX,
            loaded: false,
            globals: HashSet::default(),
            names: Names::default(),
            sorts_done: 0,
            presorts,
            collectors: Map::default(),
            schedulers: Vec::new(),
            commands: Map::default(),
            command_macros: CommandMacroRegistry::default(),
            extension_state: Map::default(),
            rust_rules: Map::default(),
            overall_run_report: RunReport::default(),
            report_level: ReportLevel::default(),
            fresh: 0,
            strict_mode: false,
            reserved_symbols: true,
            seminaive: true,
        };
        eg.rules.add_ruleset("");
        let sorts: Vec<ArcSort> = eg
            .db
            .schema()
            .sorts()
            .map(|(_, s)| s.arcsort().clone())
            .collect();
        sorts
            .into_iter()
            .for_each(|sort| sort.register_primitives(&mut eg));
        for sort in [sort::BigIntSort.to_arcsort(), sort::BigRatSort.to_arcsort()] {
            if eg.db.sort(sort.name()).is_err() {
                eg.add_arcsort(sort, span!()).expect("a new sort");
            }
        }
        eg.sync_sorts();
        eg
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    /// The database; sorts declared here get the generic primitives at the
    /// next command.
    pub fn database_mut(&mut self) -> &mut Database {
        &mut self.db
    }

    // Gives the sorts declared since the last call the primitives every
    // sort has, and the function sorts what fits them.
    fn sync_sorts(&mut self) {
        let sorts = self.db.schema().sorts().count();
        for sort in self.sorts_done..sorts {
            sort::register_generic(&mut self.db, SortId::new(sort));
        }
        self.sorts_done = sorts;
        sort::register_functions(&mut self.db);
    }

    pub(crate) fn fresh(&mut self, prefix: &str) -> String {
        self.fresh += 1;
        format!("@{prefix}{}", self.fresh)
    }

    pub fn extension_state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        let state = self.extension_state.get(&TypeId::of::<T>())?;
        state.as_ref().as_any().downcast_ref()
    }

    pub fn extension_state_or_default<T: Default + Clone + Send + Sync + 'static>(
        &mut self,
    ) -> &mut T {
        let state = self
            .extension_state
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()));
        let state = state.as_mut().as_any_mut();
        state.downcast_mut().expect("one state per type")
    }

    pub fn command_macros(&self) -> &CommandMacroRegistry {
        &self.command_macros
    }

    pub fn command_macros_mut(&mut self) -> &mut CommandMacroRegistry {
        &mut self.command_macros
    }

    pub fn add_command(
        &mut self,
        name: String,
        command: Arc<dyn UserDefinedCommand>,
    ) -> Result<(), Error> {
        let taken =
            self.db.table(&name).is_ok() || self.db.schema().primitives(&name).next().is_some();
        if taken || self.commands.contains_key(&name) {
            return Err(Error::CommandAlreadyExists(name, span!()));
        }
        self.commands.insert(name, command);
        Ok(())
    }

    /// Whether a global must start with `$`, and a rule variable must not.
    pub fn set_strict_mode(&mut self, strict_mode: bool) {
        self.strict_mode = strict_mode;
    }

    pub fn strict_mode(&self) -> bool {
        self.strict_mode
    }

    /// Whether names starting with `@`, which the e-graph uses for its own
    /// tables, are rejected.
    pub fn ensure_no_reserved_symbols(&mut self, should_ensure: bool) {
        self.reserved_symbols = should_ensure;
    }

    // Checks that `name` is free for a new sort or table.
    fn reserve(&self, name: &str, span: Span) -> Result<(), Error> {
        let taken = self.db.table(name).is_ok() || self.db.sort(name).is_ok();
        if taken || (self.reserved_symbols && name.starts_with('@')) {
            return Err(Error::Type(
                span,
                format!("`{name}` is bound already or reserved"),
            ));
        }
        Ok(())
    }

    pub fn add_sort<S: Sort>(&mut self, sort: S, span: Span) -> Result<(), Error> {
        self.add_arcsort(Arc::new(sort), span)
    }

    /// Declares an Eq sort, or the sort a presort such as `Vec` makes.
    pub fn declare_sort(
        &mut self,
        name: impl Into<String>,
        presort_and_args: &Option<(String, Vec<Expr>)>,
        span: Span,
    ) -> Result<(), Error> {
        let name = name.into();
        let sort = match presort_and_args {
            None => Arc::new(EqSort { name }),
            Some((presort, args)) => {
                let make = self
                    .presorts
                    .get(presort)
                    .ok_or_else(|| Error::Type(span, format!("no presort `{presort}`")))?;
                make(&self.db, name, args, span)?
            }
        };
        self.add_arcsort(sort, span)
    }

    pub fn add_arcsort(&mut self, sort: ArcSort, span: Span) -> Result<(), Error> {
        self.reserve(sort.name(), span)?;
        self.db.add_arcsort(sort.clone());
        sort.register_primitives(self);
        self.sync_sorts();
        Ok(())
    }

    pub fn add_presort<S: Presort>(&mut self, span: Span) -> Result<(), Error> {
        if self.presorts.contains_key(S::presort_name()) {
            return Err(Error::Type(
                span,
                format!("presort `{}` exists", S::presort_name()),
            ));
        }
        add_presort::<S>(&mut self.presorts);
        Ok(())
    }

    fn sort_ids(&self, sorts: &[ArcSort]) -> Result<Vec<SortId>, Error> {
        sorts.iter().map(|sort| self.db.sort(sort.name())).collect()
    }

    // A primitive of the sorts `signature`: inputs, then the output.
    fn add_signed(
        &mut self,
        name: &str,
        signature: &[ArcSort],
        contexts: &'static [Context],
        f: impl Fn(&mut PrimCtx<'_>, &[Value]) -> Option<Value> + Send + Sync + 'static,
    ) {
        let mut inputs = self
            .sort_ids(signature)
            .expect("the sorts of a primitive are declared");
        let output = inputs.pop().expect("a primitive has an output");
        let signature = Signature { inputs, output };
        self.db.add_primitive_in(name, signature, contexts, f);
        sort::register_functions(&mut self.db);
    }

    // Declares a table, and gives function sorts the new target.
    fn declare(
        &mut self,
        span: Span,
        name: &str,
        kind: TableKind,
        inputs: &[String],
        output: Option<&str>,
        merge: Option<&Expr>,
    ) -> Result<TableId, Error> {
        self.reserve(name, span)?;
        let sort = |name: &str| {
            self.db
                .sort(name)
                .map_err(|_| Error::Type(span, format!("no sort `{name}`")))
        };
        let keys: Vec<SortId> = inputs
            .iter()
            .map(|name| sort(name))
            .collect::<Result<_, _>>()?;
        let out = output.map(sort).transpose()?;
        let table = match (kind, out) {
            (TableKind::Relation, _) | (_, None) => self.db.add_relation(name, &keys),
            (TableKind::Constructor, Some(out)) => {
                if self.db.schema().sort(out).kind() != SortKind::Eq {
                    return Err(Error::Type(
                        span,
                        format!("constructor `{name}` must make an Eq sort"),
                    ));
                }
                self.db.add_constructor(name, &keys, out)
            }
            (TableKind::Function, Some(out)) => {
                let merge = match merge {
                    Some(merge) => self.merge(merge, out, span)?.0,
                    None => Merge::AssertEq,
                };
                self.db.add_function(name, &keys, out, merge)
            }
        };
        sort::register_functions(&mut self.db);
        Ok(table)
    }

    // The merge an expression over `old` and `new` stands for, and its sort.
    fn merge(&mut self, expr: &Expr, out: SortId, span: Span) -> Result<(Merge, SortId), Error> {
        match expr {
            Expr::Var(_, name) if name == "old" => Ok((Merge::Old, out)),
            Expr::Var(_, name) if name == "new" => Ok((Merge::New, out)),
            Expr::Lit(_, Literal::Arg(_)) => Err(Error::Type(
                span,
                "a merge reads no run argument".to_owned(),
            )),
            Expr::Lit(_, literal) => {
                let (sort, value) = self.literal(literal);
                Ok((Merge::Const(value), sort))
            }
            Expr::Call(_, head, args) => {
                let args: Vec<(Merge, SortId)> = args
                    .iter()
                    .map(|arg| self.merge(arg, out, span))
                    .collect::<Result<_, _>>()?;
                let sorts: Vec<SortId> = args.iter().map(|(_, sort)| *sort).collect();
                let merges = args.into_iter().map(|(merge, _)| merge).collect();
                let schema = self.db.schema();
                if let Some(table) = schema.table_named(head) {
                    let output = schema.table(table).columns().last().copied().unwrap_or(out);
                    return Ok((Merge::Function(table, merges), output));
                }
                // A merge runs where rule actions do.
                let fits = |&p: &ExternId| {
                    let contexts = schema.primitive_contexts(p);
                    schema.primitive(p).1.inputs == sorts && contexts.contains(&Context::Write)
                };
                let prim = schema.primitives(head).find(fits);
                let prim =
                    prim.ok_or_else(|| Error::Type(span, format!("no `{head}` fits the merge")))?;
                Ok((Merge::Prim(prim, merges), schema.primitive(prim).1.output))
            }
            Expr::Var(span, name) => Err(Error::Type(
                *span,
                format!("a merge reads `old` and `new`, not `{name}`"),
            )),
        }
    }

    fn literal(&self, literal: &Literal) -> (SortId, Value) {
        let sort = self
            .db
            .sort(sort::literal_sort(literal).name())
            .expect("a base sort");
        (sort, vm::value(self.db.base_values(), literal))
    }

    /// Runs the commands in order and returns what they print.
    pub fn run_program(&mut self, program: Vec<Command>) -> Result<Vec<CommandOutput>, Error> {
        self.sync_sorts();
        let mut outputs = Vec::new();
        for command in program {
            for command in self.command_macros.apply(command, &self.db)? {
                for command in desugar::desugar(command)? {
                    outputs.extend(self.run_command(command)?);
                }
            }
        }
        Ok(outputs)
    }

    fn run_command(&mut self, command: Command) -> Result<Vec<CommandOutput>, Error> {
        if let Some((name, span)) = declared(&command) {
            self.names.claim(name, span)?;
        }
        match command {
            Command::Sort {
                span,
                name,
                presort_and_args,
            } => {
                self.declare_sort(name, &presort_and_args, span)?;
            }
            Command::Constructor {
                span,
                name,
                schema,
                cost,
                unextractable,
            } => {
                let table = self.declare(
                    span,
                    &name,
                    TableKind::Constructor,
                    &schema.input,
                    Some(&schema.output),
                    None,
                )?;
                cost.into_iter()
                    .for_each(|cost| self.db.set_cost(table, cost));
                self.db.set_unextractable(table, unextractable);
            }
            Command::Relation { span, name, inputs } => {
                self.declare(span, &name, TableKind::Relation, &inputs, None, None)?;
            }
            Command::Function {
                span,
                name,
                schema,
                merge,
            } => {
                self.declare(
                    span,
                    &name,
                    TableKind::Function,
                    &schema.input,
                    Some(&schema.output),
                    merge.as_ref(),
                )?;
            }
            Command::AddRuleset(span, name) => {
                self.reserve_ruleset(&name, span)?;
                self.rules.add_ruleset(&name);
            }
            Command::UnstableCombinedRuleset(span, name, members) => {
                self.reserve_ruleset(&name, span)?;
                self.rules.add_combined_ruleset(&name, &members)?;
            }
            Command::Rule { rule } => self.add_rule(rule)?,
            Command::Action(Action::Let(span, name, expr)) => self.global(span, name, &expr)?,
            Command::Action(action) => _ = self.eval_actions(vec![action])?,
            Command::Extract(span, expr, variants) => {
                return self.extract_command(span, &expr, &variants);
            }
            Command::RunSchedule(schedule) => {
                return Ok(vec![CommandOutput::RunSchedule(
                    self.run_schedule(&schedule)?,
                )]);
            }
            Command::PrintOverallStatistics(_, None) => {
                return Ok(vec![CommandOutput::OverallStatistics(
                    self.overall_run_report.clone(),
                )]);
            }
            Command::PrintOverallStatistics(_, Some(path)) => {
                let output = CommandOutput::OverallStatistics(self.overall_run_report.clone());
                std::fs::write(&path, output.to_string())
                    .map_err(|e| Error::Io(format!("{path}: {e}")))?;
            }
            Command::Check(span, facts) => {
                self.check_variables(&facts, &[], span)?;
                if !self.matches(facts.clone())? {
                    return Err(Error::CheckError(facts, span));
                }
            }
            Command::PrintFunction(span, name, n, path, mode) => {
                let file = path
                    .map(|path| File::create(&path).map(|file| (file, PathBuf::from(path))))
                    .transpose();
                let file = file.map_err(|e| Error::Io(e.to_string()))?;
                return Ok(self
                    .print_function(&name, n, file, span, mode)?
                    .into_iter()
                    .collect());
            }
            Command::PrintSize(_, name) => return Ok(vec![self.print_size(name.as_deref())?]),
            Command::Fail(span, command) => {
                let names = self.names.clone();
                let result = self.run_command(*command);
                self.names = names;
                return match result {
                    Ok(_) => Err(Error::ExpectFail(span)),
                    Err(_) => Ok(Vec::new()),
                };
            }
            Command::UserDefined(_, name, args) => {
                return self.run_user_defined_command(&name, &args);
            }
            command @ (Command::Datatype { .. }
            | Command::Datatypes { .. }
            | Command::Rewrite(..)
            | Command::BiRewrite(..)) => {
                unreachable!("desugaring expands `{command}`")
            }
        }
        Ok(Vec::new())
    }

    fn reserve_ruleset(&self, name: &str, span: Span) -> Result<(), Error> {
        if self.has_ruleset(name) || (self.reserved_symbols && name.starts_with('@')) {
            return Err(Error::Type(
                span,
                format!("ruleset `{name}` exists or is reserved"),
            ));
        }
        Ok(())
    }

    pub(crate) fn has_ruleset(&self, name: &str) -> bool {
        self.rules.has_ruleset(name) || (self.loaded && self.program.rulesets().any(|r| r == name))
    }

    fn add_rule(&mut self, rule: Rule) -> Result<(), Error> {
        if self.loaded {
            return Err(Error::Unsupported(
                "a loaded program takes no new rules".to_owned(),
            ));
        }
        if !self.has_ruleset(&rule.ruleset) {
            return Err(Error::NoSuchRuleset(rule.ruleset, rule.span));
        }
        let lets = rule.head.0.iter().filter_map(|action| match action {
            Action::Let(_, name, _) => Some(name.as_str()),
            _ => None,
        });
        self.check_variables(&rule.body, &lets.collect::<Vec<_>>(), rule.span)?;
        if self
            .rules
            .rules_of(&rule.ruleset)
            .iter()
            .any(|other| other.name == rule.name)
        {
            return Err(Error::RuleAlreadyExists(rule.name, rule.span));
        }
        if self.strict_mode {
            let mut prefixed = Vec::new();
            rule.body
                .iter()
                .for_each(|fact| vars_of_fact(fact, &mut prefixed));
            if let Some(var) = prefixed
                .iter()
                .find(|v| v.starts_with(GLOBAL_NAME_PREFIX) && !self.globals.contains(*v))
            {
                return Err(Error::Type(
                    rule.span,
                    format!("non-global `{var}` starts with `{GLOBAL_NAME_PREFIX}`"),
                ));
            }
        }
        self.rules.add_command(&self.db, Command::Rule { rule })
    }

    // Checks that the variables of a query and the `lets` of its actions
    // reuse no declared name. A global read by its name is no variable.
    fn check_variables(&self, facts: &[Fact], lets: &[&str], span: Span) -> Result<(), Error> {
        let mut names = self.names.clone();
        let mut vars = Vec::new();
        facts.iter().for_each(|fact| vars_of_fact(fact, &mut vars));
        vars.sort();
        vars.dedup();
        let vars = vars
            .iter()
            .map(String::as_str)
            .filter(|v| !self.globals.contains(*v));
        vars.chain(lets.iter().copied())
            .try_for_each(|var| names.claim_variable(var, span))
    }

    // A top-level `let`: a nullary function holding the value.
    fn global(&mut self, span: Span, name: String, expr: &Expr) -> Result<(), Error> {
        if self.strict_mode && !name.starts_with(GLOBAL_NAME_PREFIX) {
            return Err(Error::Type(
                span,
                format!("global `{name}` must start with `{GLOBAL_NAME_PREFIX}`"),
            ));
        }
        self.reserve(&name, span)?;
        self.names.claim_global(&name, span)?;
        let (sort, value) = self.eval_expr(expr)?;
        let table = self
            .db
            .add_function(&name, &[], self.db.sort(sort.name())?, Merge::AssertEq);
        self.db.insert(table, &[value])?;
        self.globals.insert(name);
        Ok(())
    }

    /// The sort and value of an expression, making the terms it names.
    pub fn eval_expr(&mut self, expr: &Expr) -> Result<(ArcSort, Value), Error> {
        let span = expr.span();
        let probe = Expr::Call(span, PROBE.to_owned(), vec![expr.clone()]);
        let found = self.eval_actions(vec![Action::Expr(span, probe)])?;
        let (sort, value) = found.expect("the probe ran");
        Ok((self.db.schema().sort(sort).arcsort().clone(), value))
    }

    // Runs top-level actions at once, and returns what the probe in them
    // saw. Globals they read are looked up first.
    fn eval_actions(&mut self, actions: Vec<Action>) -> Result<Option<(SortId, Value)>, Error> {
        let span = actions.first().map_or(span!(), |action| match action {
            Action::Let(span, ..)
            | Action::Set(span, ..)
            | Action::Change(span, ..)
            | Action::Union(span, ..)
            | Action::Panic(span, ..)
            | Action::Expr(span, ..) => *span,
        });
        let rule = Rule {
            span,
            head: ast::GenericActions(actions),
            body: Vec::new(),
            name: String::new(),
            ruleset: String::new(),
            eval_mode: RuleEvalMode::Naive,
        };
        let (_, rule) = rules::typed(&self.db, rule)?;
        let result = self.db.with_ctx(|ctx| plan::hir::run_actions(&rule, ctx));
        self.db.rebuild()?;
        result
    }

    // Whether `facts` match anything.
    pub(crate) fn matches(&mut self, facts: Vec<Fact>) -> Result<bool, Error> {
        let mut rules = Rules::new();
        rules.add_query(&self.db, "@check", facts)?;
        rules.compile(&self.db)?.matches(&mut self.db, "@check")
    }

    // The primitive that records its arguments, for these sorts, that
    // queries share.
    pub(crate) fn collector(&mut self, sorts: &[SortId]) -> Sink {
        if let Some(sink) = self.collectors.get(sorts) {
            return sink.clone();
        }
        let sink = self.sink("@collect", sorts);
        self.collectors.insert(sorts.to_vec(), sink.clone());
        sink
    }

    // A primitive `name` that records the calls and arguments it gets.
    pub(crate) fn sink(&mut self, name: &str, sorts: &[SortId]) -> Sink {
        let sink: Sink = Arc::default();
        let into = sink.clone();
        let (unit, inputs) = (self.db.sort("Unit").expect("a base sort"), sorts.to_vec());
        self.db.add_primitive(
            name,
            Signature {
                inputs,
                output: unit,
            },
            move |ctx, args| {
                let mut sink = into.lock().expect("a sink lock");
                sink.0 += 1;
                sink.1.extend_from_slice(args);
                Some(ctx.base(()))
            },
        );
        sink
    }

    /// The bindings of `vars` in each match of `facts`.
    pub fn query(
        &mut self,
        vars: &[(&str, ArcSort)],
        facts: Facts,
    ) -> Result<Vec<Map<String, Value>>, Error> {
        self.check_variables(&facts.0, &[], span!())?;
        let sorts: Vec<ArcSort> = vars.iter().map(|(_, sort)| sort.clone()).collect();
        let sink = self.collector(&self.sort_ids(&sorts)?);
        let args = vars
            .iter()
            .map(|(name, _)| Expr::Var(span!(), (*name).to_owned()))
            .collect();
        let head = ast::GenericActions(vec![Action::Expr(
            span!(),
            Expr::Call(span!(), "@collect".to_owned(), args),
        )]);
        let rule = Rule {
            span: span!(),
            head,
            body: facts.0,
            name: "@query".to_owned(),
            ruleset: "@query".to_owned(),
            eval_mode: RuleEvalMode::Naive,
        };
        let mut rules = Rules::new();
        rules.add_command(&self.db, Command::Rule { rule })?;
        *sink.lock().expect("a sink lock") = (0, Vec::new());
        rules
            .compile(&self.db)?
            .run(&mut self.db, &Schedule::run("@query"), &[])?;
        let (calls, values) = std::mem::take(&mut *sink.lock().expect("a sink lock"));
        if vars.is_empty() {
            return Ok(vec![Map::default(); calls.min(1)]);
        }
        let names = vars.iter().map(|(name, _)| (*name).to_owned());
        let row = |values: &[Value]| names.clone().zip(values.iter().copied()).collect();
        Ok(values.chunks(vars.len()).map(row).collect())
    }

    pub(crate) fn run_schedule(&mut self, schedule: &Schedule) -> Result<RunReport, Error> {
        self.check_rulesets(schedule)?;
        self.rules.plan_schedule(&self.db, schedule)?;
        self.plan()?;
        self.program.set_naive(!self.seminaive);
        let report = self.program.run(&mut self.db, schedule, &[])?;
        self.overall_run_report.union(report.clone());
        Ok(report)
    }

    fn check_rulesets(&self, schedule: &Schedule) -> Result<(), Error> {
        match schedule {
            Schedule::Run(span, config) if !self.has_ruleset(&config.ruleset) => {
                Err(Error::NoSuchRuleset(config.ruleset.clone(), *span))
            }
            Schedule::Run(..) => Ok(()),
            Schedule::Repeat(_, _, schedule) | Schedule::Saturate(_, schedule) => {
                self.check_rulesets(schedule)
            }
            Schedule::Sequence(_, schedules) => {
                schedules.iter().try_for_each(|s| self.check_rulesets(s))
            }
        }
    }

    // Plans the rules again if they changed, keeping when each ran.
    pub(crate) fn plan(&mut self) -> Result<(), Error> {
        if self.loaded || self.planned == self.rules.count() {
            return Ok(());
        }
        self.program = self.rules.recompile(&self.db, &self.program)?;
        self.program.set_report_level(self.report_level);
        self.planned = self.rules.count();
        Ok(())
    }

    /// One iteration of a ruleset.
    pub fn step_rules(&mut self, ruleset: &str) -> Result<RunReport, Error> {
        if !self.has_ruleset(ruleset) {
            return Err(Error::NoSuchRuleset(ruleset.to_owned(), span!()));
        }
        self.plan()?;
        let iteration = self.program.step(&mut self.db, &[ruleset], true)?;
        Ok(RunReport::of(ruleset, iteration, self.report_level))
    }

    pub fn set_ruleset_mode(&mut self, ruleset: &str, mode: Mode) {
        self.rules.set_mode(ruleset, mode);
        self.planned = usize::MAX;
    }

    /// The program file of the rules, as [`Program::to_bytes`] writes it.
    pub fn save_program(&mut self) -> Vec<u8> {
        self.plan().expect("typed rules plan");
        self.program.to_bytes()
    }

    /// Runs the program in `bytes` from now on, in place of the rules.
    /// Callbacks of rust rules must be bound before.
    pub fn load_program(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let file = vm::decode(bytes).map_err(Error::Load)?;
        for decl in file
            .sorts
            .iter()
            .filter(|decl| decl.class == vm::SortClass::Eq)
        {
            if self.db.sort(&decl.name).is_err() {
                self.db.add_sort(&decl.name, SortKind::Eq);
            }
        }
        for decl in &file.externs {
            let Some(f) = decl
                .name
                .strip_prefix("@rust:")
                .and_then(|name| self.rust_rules.get(name))
            else {
                continue;
            };
            let sort = |s: &u32| self.db.sort(&file.sorts[*s as usize].name);
            let inputs = decl.inputs.iter().map(sort).collect::<Result<_, _>>()?;
            let (output, f) = (sort(&decl.output)?, f.clone());
            self.db.add_primitive(
                &decl.name,
                Signature { inputs, output },
                move |ctx, args| f(ctx, args),
            );
        }
        self.program = Program::from_bytes(bytes, &mut self.db).map_err(Error::Load)?;
        self.program.set_report_level(self.report_level);
        (self.loaded, self.rules) = (true, Rules::new());
        self.sync_sorts();
        Ok(())
    }

    /// The callback of the rust rule `name` of a program loaded later.
    pub fn bind_rust_rule(
        &mut self,
        name: &str,
        func: impl for<'a, 'db> Fn(WriteState<'a, 'db>, &[Value]) -> Option<()>
        + Clone
        + Send
        + Sync
        + 'static,
    ) {
        let f: Arc<PrimFn> = Arc::new(move |ctx, args| {
            func(WriteState::new(ctx), args)?;
            Some(ctx.base(()))
        });
        self.rust_rules.insert(name.to_owned(), f);
    }

    // Adds a rule whose action calls `f` with the bindings of `vars`.
    pub(crate) fn rust_rule(
        &mut self,
        name: &str,
        ruleset: &str,
        vars: &[(&str, ArcSort)],
        facts: Facts,
        naive: bool,
        f: Arc<PrimFn>,
    ) -> Result<Vec<CommandOutput>, Error> {
        let prim = format!("@rust:{name}");
        if self.db.schema().primitives(&prim).next().is_some() {
            return Err(Error::RuleAlreadyExists(name.to_owned(), span!()));
        }
        let sorts: Vec<ArcSort> = vars
            .iter()
            .map(|(_, sort)| sort.clone())
            .chain([sort::literal_sort(&Literal::Unit)])
            .collect();
        let contexts = if naive {
            FullState::valid_contexts()
        } else {
            WriteState::valid_contexts()
        };
        self.add_signed(&prim, &sorts, contexts, move |ctx, args| f(ctx, args));
        let args = vars
            .iter()
            .map(|(var, _)| Expr::Var(span!(), (*var).to_owned()))
            .collect();
        let head =
            ast::GenericActions(vec![Action::Expr(span!(), Expr::Call(span!(), prim, args))]);
        let eval_mode = if naive {
            RuleEvalMode::Naive
        } else {
            RuleEvalMode::Seminaive
        };
        let rule = Rule {
            span: span!(),
            head,
            body: facts.0,
            name: name.to_owned(),
            ruleset: ruleset.to_owned(),
            eval_mode,
        };
        self.run_program(vec![Command::Rule { rule }])
    }

    pub fn get_function_names(&self) -> Vec<String> {
        self.functions_iter().map(|(name, _)| name).collect()
    }

    /// Every table the e-graph did not make for itself, in declaration order.
    pub fn functions_iter(&self) -> impl Iterator<Item = (String, Function)> + '_ {
        let schema = self.db.schema();
        let tables = schema.tables().filter(|(_, t)| !t.name().starts_with('@'));
        tables.map(move |(_, t)| (t.name().to_owned(), self.function_of(t)))
    }

    fn function_of(&self, schema: &TableSchema) -> Function {
        Function {
            func_type: FuncType::of(schema, &self.db.sorts),
            let_binding: self.globals.contains(schema.name()),
            cost: schema.cost(),
            unextractable: schema.is_unextractable(),
        }
    }

    pub fn get_function(&self, name: &str) -> Option<Function> {
        let schema = self.db.schema();
        Some(self.function_of(schema.table(schema.table_named(name)?)))
    }

    /// Runs `f` on a state that reads the database.
    pub fn read<R>(&self, f: impl FnOnce(ReadState<'_, '_>) -> R) -> R {
        f(ReadState::of_view(self.db.view()))
    }

    pub fn function_entries(
        &self,
        name: &str,
        f: impl FnMut(FunctionEntry<'_>),
    ) -> Result<(), Error> {
        self.read(|state| state.function_entries(name, f))
    }

    pub fn function_entries_while(
        &self,
        name: &str,
        f: impl FnMut(FunctionEntry<'_>) -> bool,
    ) -> Result<(), Error> {
        self.read(|state| state.function_entries_while(name, f))
    }

    pub fn constructor_enodes(&self, name: &str, f: impl FnMut(Enode<'_>)) -> Result<(), Error> {
        self.read(|state| state.constructor_enodes(name, f))
    }

    pub fn constructor_enodes_while(
        &self,
        name: &str,
        f: impl FnMut(Enode<'_>) -> bool,
    ) -> Result<(), Error> {
        self.read(|state| state.constructor_enodes_while(name, f))
    }

    /// Drops every row of the table.
    pub fn clear_function(&mut self, name: &str) -> Result<(), Error> {
        let table = self.db.table(name)?;
        self.db.clear(table);
        Ok(())
    }

    /// Runs `f` on a state that reads and writes; the writes apply when it
    /// returns, so its reads do not see them.
    pub fn update<R>(
        &mut self,
        f: impl FnOnce(FullState<'_, '_>) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let result = self.db.with_ctx(|ctx| f(FullState::new(ctx)));
        if self.db.needs_rebuild() {
            self.db.rebuild()?;
        }
        result
    }

    pub fn num_tuples(&self) -> usize {
        self.functions_iter()
            .map(|(name, _)| self.get_size(&name))
            .sum()
    }

    /// The rows of the table; panics if there is none of this name.
    pub fn get_size(&self, name: &str) -> usize {
        self.db
            .rows(self.db.table(name).expect("a declared table"))
            .count()
    }

    pub fn total_size(&self) -> usize {
        self.num_tuples()
    }

    /// The rows of constructors to Eq sorts.
    pub fn num_nodes(&self) -> usize {
        let constructors = self.functions_iter().filter(|(_, f)| {
            f.func_type.subtype == FunctionSubtype::Constructor && f.func_type.output.is_eq_sort()
        });
        constructors.map(|(name, _)| self.get_size(&name)).sum()
    }

    pub fn get_sort_by<S: Sort>(&self, f: impl Fn(&Arc<S>) -> bool) -> Arc<S> {
        let mut sorts = self.get_sorts_by(f);
        assert_eq!(
            sorts.len(),
            1,
            "one sort of type {}",
            std::any::type_name::<S>()
        );
        sorts.remove(0)
    }

    pub fn get_sort<S: Sort>(&self) -> Arc<S> {
        self.get_sort_by(|_| true)
    }

    pub fn get_sorts_by<S: Sort>(&self, f: impl Fn(&Arc<S>) -> bool) -> Vec<Arc<S>> {
        let sorts = self
            .db
            .schema()
            .sorts()
            .filter_map(|(_, s)| s.arcsort().clone().as_arc_any().downcast::<S>().ok());
        sorts.filter(f).collect()
    }

    pub fn get_sorts<S: Sort>(&self) -> Vec<Arc<S>> {
        self.get_sorts_by(|_| true)
    }

    pub fn get_arcsorts_by(&self, f: impl Fn(&ArcSort) -> bool) -> Vec<ArcSort> {
        self.db
            .schema()
            .sorts()
            .map(|(_, s)| s.arcsort().clone())
            .filter(f)
            .collect()
    }

    pub fn get_arcsort_by(&self, f: impl Fn(&ArcSort) -> bool) -> ArcSort {
        let mut sorts = self.get_arcsorts_by(f);
        assert_eq!(sorts.len(), 1, "one sort fits");
        sorts.remove(0)
    }

    /// The sort whose values are of the Rust type `T`.
    pub fn get_arcsort_for_value_type<T: 'static>(&self) -> ArcSort {
        self.get_arcsort_by(|sort| sort.value_type() == Some(TypeId::of::<T>()))
    }

    pub fn get_sort_by_name(&self, name: &str) -> Option<&ArcSort> {
        let schema = self.db.schema();
        Some(schema.sort(schema.sort_named(name)?).arcsort())
    }

    pub fn get_overall_run_report(&self) -> &RunReport {
        &self.overall_run_report
    }

    pub fn value_to_base<T: BaseValue>(&self, x: Value) -> T {
        self.db.base_value(x)
    }

    pub fn base_to_value<T: BaseValue>(&self, x: T) -> Value {
        self.db.base(x)
    }

    pub fn value_to_container<T: ContainerValue>(&self, x: Value) -> Option<&T> {
        self.db.container_value(x)
    }

    pub fn container_to_value<T: ContainerValue>(&mut self, x: T) -> Value {
        self.db.container(x)
    }

    pub fn container_inner_values(&self, sort: &ArcSort, value: Value) -> Vec<(ArcSort, Value)> {
        sort.inner_values(self.db.container_values(), value)
    }

    pub fn reconstruct_base_value(
        &self,
        sort: &ArcSort,
        value: Value,
        termdag: &mut TermDag,
    ) -> TermId {
        sort.reconstruct_termdag_base(self.db.base_values(), value, termdag)
    }

    pub fn reconstruct_container_value(
        &self,
        sort: &ArcSort,
        value: Value,
        termdag: &mut TermDag,
        element_terms: Vec<TermId>,
    ) -> TermId {
        sort.reconstruct_termdag_container(
            self.db.container_values(),
            value,
            termdag,
            element_terms,
        )
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    pub fn run_user_defined_command(
        &mut self,
        name: &str,
        args: &[Expr],
    ) -> Result<Vec<CommandOutput>, Error> {
        let command = self
            .commands
            .get(name)
            .cloned()
            .ok_or_else(|| Error::UnknownName(name.to_owned()))?;
        command.update(self, args)
    }

    /// What runs report; see [`ReportLevel`].
    pub fn set_report_level(&mut self, level: ReportLevel) {
        self.report_level = level;
        self.program.set_report_level(level);
    }

    /// Prints the size of every table to standard error.
    pub fn dump_debug_info(&self) {
        for (_, table) in self.db.schema().tables() {
            let rows = self
                .db
                .rows(self.db.table(table.name()).expect("a table"))
                .count();
            eprintln!("{}: {rows} rows", table.name());
        }
    }

    /// Prints up to `n` rows of a function, to `file` if given.
    pub fn print_function(
        &mut self,
        name: &str,
        n: Option<usize>,
        file: Option<(File, PathBuf)>,
        _span: Span,
        mode: PrintFunctionMode,
    ) -> Result<Option<CommandOutput>, Error> {
        let (terms, outputs, termdag) =
            self.function_to_dag(name, n.unwrap_or(usize::MAX), true)?;
        let function = self.get_function(name).expect("function_to_dag found it");
        let rows = terms
            .into_iter()
            .zip(outputs.expect("outputs were asked for"))
            .collect();
        let output = CommandOutput::PrintFunction(function, termdag, rows, mode);
        let Some((mut file, path)) = file else {
            return Ok(Some(output));
        };
        let written = file.write_all(output.to_string().as_bytes());
        written.map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        Ok(None)
    }

    /// The size of a function, or of each function but globals.
    pub fn print_size(&self, name: Option<&str>) -> Result<CommandOutput, Error> {
        if let Some(name) = name {
            let found = self
                .get_function(name)
                .filter(|f| !f.is_hidden() && !f.let_binding);
            found.ok_or_else(|| Error::UnknownName(name.to_owned()))?;
            return Ok(CommandOutput::PrintFunctionSize(self.get_size(name)));
        }
        let mut sizes: Vec<(String, usize)> = self
            .functions_iter()
            .filter(|(_, f)| !f.let_binding)
            .map(|(name, _)| {
                let size = self.get_size(&name);
                (name, size)
            })
            .collect();
        sizes.sort();
        Ok(CommandOutput::PrintAllFunctionsSize(sizes))
    }
}

// The sort, table or ruleset a command declares, with its span.
fn declared(command: &Command) -> Option<(&str, Span)> {
    match command {
        Command::Sort { span, name, .. }
        | Command::Constructor { span, name, .. }
        | Command::Relation { span, name, .. }
        | Command::Function { span, name, .. }
        | Command::AddRuleset(span, name)
        | Command::UnstableCombinedRuleset(span, name, _) => Some((name, *span)),
        _ => None,
    }
}

fn vars_of_fact(fact: &Fact, out: &mut Vec<String>) {
    fn walk(expr: &Expr, out: &mut Vec<String>) {
        match expr {
            Expr::Var(_, name) => out.push(name.clone()),
            Expr::Call(_, _, args) => args.iter().for_each(|arg| walk(arg, out)),
            Expr::Lit(..) => {}
        }
    }
    match fact {
        Fact::Eq(_, a, b) => [a, b].into_iter().for_each(|e| walk(e, out)),
        Fact::Fact(e) => walk(e, out),
    }
}
