//! Executable lowering against an explicitly shared engine schema.
mod program;

use program::{Dependencies, Selection};

use std::collections::{BTreeMap, BTreeSet};

use tir_egglog::{Database, EGraph, Merge, Rules, Schedule, SortId, SortKind, TableKind, ast};

use crate::{
    CheckedProgram, Error,
    ast::{BinaryOp, Binding, Declaration, Expr, Expression, Literal, Path, RuleBinder, UnaryOp},
    lower::{CheckedMatch, PortIndex, PortType},
};

/// Reusable schema and host bindings, shared by independently compiled programs.
pub struct Schema {
    engine: EGraph,
    hosts: BTreeSet<Path>,
    functions: BTreeSet<Path>,
    constructors: BTreeSet<Path>,
    named_relations: BTreeMap<String, Vec<String>>,
}

fn error(error: impl std::fmt::Display) -> Error {
    Error(error.to_string())
}
fn path_name(path: &[String]) -> String {
    path.join("::")
}
fn constructor_name(path: &[String]) -> String {
    path_name(&path[path.len().saturating_sub(2)..])
}

fn ir_identity(path: &[String]) -> bool {
    matches!(path, [std, ir, name] if std == "std" && ir == "ir" && matches!(
        name.as_str(), "operation_named" | "operation_kind" | "operation_name" |
        "operation_key" | "value_key" | "value_id" | "value_ref_key" |
        "raw_operation_id" | "raw_value_id"
    ))
}

fn parts(expr: &Expr) -> &[Expr] {
    match &expr.value {
        Expression::Tuple(items) => items,
        Expression::Unit => &[],
        _ => std::slice::from_ref(expr),
    }
}
fn path(expr: &Expr) -> Result<&Path, Error> {
    match &expr.value {
        Expression::Path(path) => Ok(path),
        _ => Err(Error(
            "executable schema requires a named type or value".into(),
        )),
    }
}
fn arrow(expr: &Expr) -> Option<(&Expr, &Expr)> {
    match &expr.value {
        Expression::Binary {
            op: BinaryOp::Arrow,
            left,
            right,
        } => Some((left, right)),
        _ => None,
    }
}
fn table_type(ty: &Expr) -> Option<&[Expr]> {
    match &ty.value {
        Expression::Generic { value, arguments }
            if path(value).is_ok_and(|path| path == &["std", "table", "Table"]) =>
        {
            Some(arguments)
        }
        _ => None,
    }
}
fn proposition(expr: &Expr) -> bool {
    matches!(&expr.value, Expression::Path(path) if path == &["Prop"])
}

fn storage_name(declaration: &Declaration) -> Option<&str> {
    let Declaration::Axiom { name, ty, .. } = declaration else {
        return None;
    };
    if table_type(ty).is_none() && !arrow(ty).is_some_and(|(_, output)| proposition(output)) {
        return None;
    }
    Some(name)
}

fn ir_function(expr: &Expr) -> Option<&str> {
    let Expression::Path(path) = &expr.value else {
        return None;
    };
    let [std, ir, function] = path.as_slice() else {
        return None;
    };
    (std == "std" && ir == "ir").then_some(function.as_str())
}

fn static_match(expr: &Expr) -> Option<&Expr> {
    match &expr.value {
        Expression::Member { value, name } if name == "matches" => Some(value),
        _ => None,
    }
}

fn clause(expr: &Expr) -> bool {
    match &expr.value {
        Expression::Call { .. }
        | Expression::RecordCall { .. }
        | Expression::Unary {
            op: UnaryOp::Not, ..
        }
        | Expression::Binary {
            op: BinaryOp::Equal | BinaryOp::NotEqual,
            ..
        } => true,
        Expression::Tuple(items) => items.iter().any(clause),
        Expression::Binary {
            op: BinaryOp::Arrow,
            left,
            ..
        } => clause(left),
        _ => false,
    }
}

fn propositions(expr: &Expr) -> Vec<&Expr> {
    let mut pending = vec![expr];
    let mut result = Vec::new();
    while let Some(expr) = pending.pop() {
        match &expr.value {
            Expression::Tuple(items) => pending.extend(items.iter().rev()),
            _ => result.push(expr),
        }
    }
    result
}

impl Schema {
    pub(crate) fn from_engine(engine: EGraph) -> Self {
        Self {
            engine,
            hosts: BTreeSet::new(),
            functions: BTreeSet::new(),
            constructors: BTreeSet::new(),
            named_relations: BTreeMap::new(),
        }
    }

    /// Install each relation and table under its declared name.
    pub fn new(program: &CheckedProgram) -> Result<Self, Error> {
        let mut schema = Self::from_engine(EGraph::default());
        schema.install(
            &Dependencies::collect(program, &[], Selection::FullSchema),
            true,
        )?;
        Ok(schema)
    }

    /// Check a program against an existing engine, including its native functions.
    pub fn with_engine(program: &CheckedProgram, engine: EGraph) -> Result<Self, Error> {
        let mut schema = Self::from_engine(engine);
        schema.extend(program)?;
        Ok(schema)
    }

    /// The database whose declarations type and bind every compiled program.
    pub fn database(&self) -> &Database {
        self.engine.database()
    }

    /// Install additional checked declarations without importing their rules.
    pub fn extend(&mut self, program: &CheckedProgram) -> Result<(), Error> {
        self.install(
            &Dependencies::collect(program, &[], Selection::FullSchema),
            false,
        )
    }

    fn install(
        &mut self,
        dependencies: &Dependencies<'_>,
        generate_hosts: bool,
    ) -> Result<(), Error> {
        let mut storage = BTreeMap::new();
        for path in &dependencies.ordered {
            if let Some(name) = storage_name(dependencies.declarations[path])
                && let Some(previous) = storage.insert(name, path)
            {
                return Err(Error(format!(
                    "storage {name} is declared by both {} and {}",
                    path_name(previous),
                    path_name(path)
                )));
            }
        }
        let declarations = dependencies
            .ordered
            .iter()
            .filter(|path| path.first().is_none_or(|part| part != "std"))
            .map(|path| (path, dependencies.declarations[path]));
        for (full, declaration) in declarations.clone() {
            if let Declaration::Axiom { generics, ty, .. } = declaration
                && matches!(&ty.value, Expression::Path(path) if path == &["Type"])
            {
                if !generics.is_empty() {
                    return Err(Error(
                        "generic executable sorts require specialization".into(),
                    ));
                }
                let name = self.name(full);
                if self.database().schema().sort_named(&name).is_none() {
                    self.engine.database_mut().add_sort(&name, SortKind::Eq);
                }
            }
            if let Declaration::Enum { generics, .. } = declaration {
                if !generics.is_empty() {
                    return Err(Error(
                        "generic executable enums require specialization".into(),
                    ));
                }
                let name = self.name(full);
                if self.database().schema().sort_named(&name).is_none() {
                    self.engine.database_mut().add_sort(&name, SortKind::Eq);
                }
            }
        }
        for (full, declaration) in declarations.clone() {
            if let Declaration::Enum { variants, .. } = declaration {
                let out = self.sort_path(full)?;
                if self.database().schema().sort(out).kind() != SortKind::Eq {
                    return Err(Error(format!(
                        "enum {} requires an equivalence sort",
                        self.name(full)
                    )));
                }
                for variant in variants {
                    if !variant.generics.is_empty() || variant.ty.is_some() {
                        return Err(Error(
                            "indexed executable constructors require specialization".into(),
                        ));
                    }
                    let mut variant_path = full.clone();
                    variant_path.push(variant.name.clone());
                    let args = variant
                        .arguments
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(|arg| self.sort(arg))
                        .collect::<Result<Vec<_>, _>>()?;
                    let name = constructor_name(&variant_path);
                    if let Some(id) = self.database().schema().table_named(&name) {
                        let table = self.database().schema().table(id);
                        let mut columns = args.clone();
                        columns.push(out);
                        if table.kind() != TableKind::Constructor || table.columns() != columns {
                            return Err(Error(format!("incompatible constructor {name}")));
                        }
                    } else {
                        self.engine
                            .database_mut()
                            .add_constructor(&name, &args, out);
                    }
                    self.constructors.insert(variant_path);
                }
            }
        }
        // Table descriptors declare keyed storage. Proposition declarations can
        // expose those same rows without changing their functional dependency.
        for (full, declaration) in declarations.clone() {
            if let Declaration::Axiom { ty, .. } = declaration
                && let Some(arguments) = table_type(ty)
            {
                let [key, out, merge] = arguments else {
                    return Err(Error("Table requires key, value and merge".into()));
                };
                let keys = parts(key)
                    .iter()
                    .map(|ty| self.sort(ty))
                    .collect::<Result<Vec<_>, _>>()?;
                let out = self.sort(out)?;
                let merge = match path(merge)?.last().map(String::as_str) {
                    Some("Unique") => Merge::AssertEq,
                    Some("Union") => {
                        if matches!(self.database().schema().sort(out).kind(), SortKind::Base(_)) {
                            return Err(Error(
                                "Union merge requires values with equivalence identities".into(),
                            ));
                        }
                        Merge::Union
                    }
                    _ => return Err(Error("unsupported executable table merge".into())),
                };
                let name = self.name(full);
                if let Some(id) = self.database().schema().table_named(&name) {
                    let table = self.database().schema().table(id);
                    let mut columns = keys.clone();
                    columns.push(out);
                    if table.kind() != TableKind::Function
                        || table.columns() != columns
                        || table.key_arity() != keys.len()
                        || table.merge() != Some(&merge)
                    {
                        return Err(Error(format!("incompatible function {name}")));
                    }
                } else {
                    self.engine
                        .database_mut()
                        .add_function(&name, &keys, out, merge);
                }
                self.functions.insert(full.clone());
            }
        }
        for (full, declaration) in declarations {
            let Declaration::Axiom { ty, .. } = declaration else {
                continue;
            };
            if let Some((args, result)) = arrow(ty)
                && proposition(result)
            {
                self.install_relation(full.clone(), args)?;
            }
        }
        if generate_hosts {
            self.declare_hosts(dependencies)?;
        }
        Ok(())
    }

    fn declare_hosts(&mut self, dependencies: &Dependencies<'_>) -> Result<(), Error> {
        for path in &dependencies.needed {
            let Declaration::Axiom { generics, ty, .. } = dependencies.declarations[path] else {
                continue;
            };
            let Some((inputs, output)) = arrow(ty).filter(|(_, output)| !proposition(output))
            else {
                continue;
            };
            if !generics.is_empty() || clause(ty) || ir_identity(path) {
                continue;
            }
            let Ok(inputs) = parts(inputs)
                .iter()
                .map(|ty| self.sort(ty))
                .collect::<Result<Vec<_>, _>>()
            else {
                continue;
            };
            let Ok(output) = self.sort(output) else {
                continue;
            };
            let signature = tir_egglog::Signature { inputs, output };
            let name = self.name(path);
            let schema = self.database().schema();
            if !schema
                .primitives(&name)
                .any(|id| schema.primitive(id).1 == &signature)
            {
                self.engine
                    .database_mut()
                    .add_primitive(&name, signature, |_, _| None);
                self.hosts.insert(path.clone());
            }
        }
        Ok(())
    }

    fn install_relation(&mut self, path: Path, inputs: &Expr) -> Result<(), Error> {
        let name = self.name(&path);
        let (fields, columns) = if let Expression::Record(fields) = &inputs.value {
            let (names, sorts) = fields
                .iter()
                .map(|(field, ty)| Ok((field.clone(), self.sort(ty)?)))
                .collect::<Result<(Vec<_>, Vec<_>), Error>>()?;
            (Some(names), sorts)
        } else {
            let sorts = parts(inputs)
                .iter()
                .map(|ty| self.sort(ty))
                .collect::<Result<Vec<_>, _>>()?;
            (None, sorts)
        };
        if let Some(fields) = &fields
            && self
                .named_relations
                .get(&name)
                .is_some_and(|installed| installed != fields)
        {
            return Err(Error(format!("incompatible named columns for {name}")));
        }
        if let Some(id) = self.database().schema().table_named(&name) {
            let table = self.database().schema().table(id);
            if table.columns() != columns
                || table.kind() == TableKind::Constructor
                || (table.kind() == TableKind::Function && fields.is_some())
            {
                return Err(Error(format!("incompatible declaration {name}")));
            }
            if table.kind() == TableKind::Function {
                self.functions.insert(path);
                return Ok(());
            }
        } else {
            self.engine.database_mut().add_relation(&name, &columns);
        }
        if let Some(fields) = fields {
            self.named_relations.insert(name, fields);
        }
        Ok(())
    }

    /// Serialize declarations independently of executable programs.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(Rules::compile_schema(self.database())
            .map_err(error)?
            .to_bytes())
    }

    pub(crate) fn compile_bytes(&self, program: &CheckedProgram) -> Result<Vec<u8>, Error> {
        Ok(self.lower(program)?.compile(self.database())?.to_bytes())
    }

    fn name(&self, path: &[String]) -> String {
        if self.constructors.contains(path) {
            return constructor_name(path);
        }
        path.last().cloned().unwrap_or_default()
    }

    fn sort_path(&self, path: &[String]) -> Result<SortId, Error> {
        let name = match path {
            [name] if name == "Int" => "i64".into(),
            [name] if name == "Bool" => "bool".into(),
            [name] if name == "String" => "String".into(),
            [std, ir, name]
                if std == "std"
                    && ir == "ir"
                    && matches!(name.as_str(), "OperationKind" | "BinaryIntegerOperation") =>
            {
                "String".into()
            }
            [std, ir, name]
                if std == "std"
                    && ir == "ir"
                    && matches!(name.as_str(), "IntegerRef" | "OperationId" | "ValueId") =>
            {
                "i64".into()
            }
            _ => self.name(path),
        };
        self.database().sort(&name).map_err(error)
    }
    fn sort(&self, expr: &Expr) -> Result<SortId, Error> {
        if let Expression::Generic { value, .. } = &expr.value {
            let family = path(value)?;
            if family == &["std", "ir", "OperationKindOf"] {
                return self.database().sort("String").map_err(error);
            }
            if family == &["std", "ir", "ValueRef"] {
                return self.database().sort("i64").map_err(error);
            }
        }
        self.sort_path(path(expr)?)
    }

    /// Lower checked source into typed engine rules without planning or loading.
    pub fn lower(&self, program: &CheckedProgram) -> Result<LoweredProgram, Error> {
        self.lower_selected(program, &[])
    }

    pub(crate) fn lower_selected(
        &self,
        program: &CheckedProgram,
        selected: &[String],
    ) -> Result<LoweredProgram, Error> {
        self.lower_dependencies(
            program,
            &Dependencies::collect(program, selected, Selection::Program),
        )
    }

    fn lower_dependencies(
        &self,
        program: &CheckedProgram,
        dependencies: &Dependencies<'_>,
    ) -> Result<LoweredProgram, Error> {
        let operations = program
            .artifact()
            .all_modules()
            .flat_map(|module| {
                module.ast.declarations.iter().filter_map(|declaration| {
                    let Declaration::Let { binding, .. } = &declaration.value else {
                        return None;
                    };
                    let name = binding.operation_name(&module.path)?;
                    let path = [module.path.as_slice(), &binding.path].concat();
                    Some((path, (name, binding)))
                })
            })
            .collect();
        let mut rules = Rules::new();
        let mut modules = BTreeMap::new();
        let mut definitions = BTreeMap::new();
        for full in &dependencies.ordered {
            let module = &full[..full.len() - 1];
            if module.first().is_some_and(|name| name == "std") {
                continue;
            }
            let ruleset = path_name(module);
            let declaration = dependencies.declarations[full];
            if let Declaration::Let { binding, .. } = declaration {
                if !binding.generics.is_empty() {
                    return Err(Error(format!(
                        "unsupported executable generic definition {}",
                        self.name(full)
                    )));
                }
                if !matches!(&binding.value.value, Expression::Function(function)
                    if function.result.as_ref().is_some_and(|ty| path(ty).is_ok_and(|p| p == &["Prop"])))
                {
                    definitions.insert(full.clone(), binding.value.clone());
                }
                continue;
            }
            let Declaration::Axiom {
                name, generics, ty, ..
            } = declaration
            else {
                continue;
            };
            if !clause(ty) {
                if dependencies.active.contains(full) {
                    self.check_axiom(full, ty)?;
                }
                continue;
            }
            rules.add_ruleset(&ruleset);
            modules.insert(module.to_vec(), ruleset.clone());
            let (premise, head) = arrow(ty).map_or((None, ty), |(a, b)| (Some(a), b));
            let lowering = RuleLowering {
                schema: self,
                operations: &operations,
                declarations: &dependencies.declarations,
                binders: generics,
                operation_matches: program.operation_matches.get(full),
            };
            let mut facts = Vec::new();
            if let Some(premise) = premise {
                let expanded = propositions(premise)
                    .into_iter()
                    .map(|expr| lowering.predicate_row(expr))
                    .collect::<Result<Vec<_>, _>>()?;
                let clauses = expanded
                    .iter()
                    .map(|expr| expr.as_ref())
                    .collect::<Vec<_>>();
                lowering
                    .validate_inputs(&clauses)
                    .map_err(|error| Error(format!("{ruleset}::{name}: {error}")))?;
                for expr in clauses {
                    lowering
                        .fact(expr, &mut facts)
                        .map_err(|error| Error(format!("{ruleset}::{name}: {error}")))?;
                }
            }
            let actions = propositions(head)
                .iter()
                .map(|expr| lowering.action(expr))
                .collect::<Result<Vec<_>, _>>()?;
            rules
                .rule(
                    self.database(),
                    &ruleset,
                    ast::Facts(facts),
                    ast::GenericActions(actions),
                )
                .map_err(error)?;
        }
        let mut schedules = BTreeMap::new();
        for (path, expression) in &definitions {
            let schedule = self.schedule(
                expression,
                &path_name(path),
                &modules,
                &definitions,
                &mut BTreeSet::from([path.clone()]),
                &mut rules,
            )?;
            schedules.insert(path.clone(), schedule);
        }
        Ok(LoweredProgram { rules, schedules })
    }

    fn check_axiom(&self, path: &[String], ty: &Expr) -> Result<(), Error> {
        if matches!(&ty.value, Expression::Path(path) if path == &["Type"])
            || table_type(ty).is_some()
            || arrow(ty).is_some_and(|(_, output)| proposition(output))
        {
            return Ok(());
        }
        if let Some((inputs, output)) = arrow(ty) {
            let inputs = parts(inputs)
                .iter()
                .map(|ty| self.sort(ty))
                .collect::<Result<Vec<_>, _>>()?;
            let output = self.sort(output)?;
            let schema = self.database().schema();
            if schema.primitives(&self.name(path)).any(|id| {
                let signature = schema.primitive(id).1;
                signature.inputs == inputs && signature.output == output
            }) {
                return Ok(());
            }
        }
        Err(Error(format!(
            "unsupported executable axiom declaration: {}",
            path_name(path)
        )))
    }

    fn bundle(
        &self,
        expression: &Expr,
        modules: &BTreeMap<Path, String>,
    ) -> Result<Vec<String>, Error> {
        match &expression.value {
            Expression::Path(path) => {
                let names = modules
                    .iter()
                    .filter(|(module, _)| module.starts_with(path))
                    .map(|(_, name)| name.clone())
                    .collect::<Vec<_>>();
                if names.is_empty() {
                    return Err(Error(format!(
                        "{} is not a selected executable rule module",
                        self.name(path)
                    )));
                }
                Ok(names)
            }
            Expression::Member { value, name } => {
                let target = [path(value)?.as_slice(), std::slice::from_ref(name)].concat();
                modules
                    .get(&target)
                    .cloned()
                    .map(|name| vec![name])
                    .ok_or_else(|| {
                        Error(format!(
                            "{} is not a generated rule bundle",
                            path_name(&target)
                        ))
                    })
            }
            Expression::Tuple(items) => items
                .iter()
                .map(|item| self.bundle(item, modules))
                .collect::<Result<Vec<_>, _>>()
                .map(|parts| parts.into_iter().flatten().collect()),
            _ => Err(Error(
                "executable rule bundles require modules or tuples of modules".into(),
            )),
        }
    }

    fn schedule(
        &self,
        expression: &Expr,
        name: &str,
        modules: &BTreeMap<Path, String>,
        definitions: &BTreeMap<Path, Expr>,
        active: &mut BTreeSet<Path>,
        rules: &mut Rules,
    ) -> Result<Schedule, Error> {
        if let Expression::Path(path) = &expression.value {
            if !active.insert(path.clone()) {
                return Err(Error("cyclic executable schedule".into()));
            }
            let expression = definitions.get(path).ok_or_else(|| {
                Error(format!("{} is not a schedule definition", self.name(path)))
            })?;
            let result = self.schedule(expression, name, modules, definitions, active, rules);
            active.remove(path);
            return result;
        }
        let Expression::Call { value, arguments } = &expression.value else {
            return Err(Error(format!("unsupported executable definition {name}")));
        };
        let callee = path(value)?.iter().map(String::as_str).collect::<Vec<_>>();
        match (callee.as_slice(), arguments.as_slice()) {
            (["std", "schedule", "Schedule", "Rules"], [bundle])
            | (["std", "schedule", "Schedule", "Repeat"], [_, bundle])
            | (["std", "schedule", "Schedule", "Saturate"], [bundle])
            | (["std", "schedule", "saturate"], [bundle])
            | (["std", "schedule", "repeat_colored"], [_, bundle]) => {
                let mut members = self.bundle(bundle, modules)?;
                members.sort();
                members.dedup();
                if callee.last() == Some(&"repeat_colored") {
                    for member in &members {
                        rules.set_colored(member);
                    }
                }
                rules.add_combined_ruleset(name, &members).map_err(error)?;
                let run = Schedule::run(name);
                match callee.last() {
                    Some(&"Rules") => Ok(run),
                    Some(&("Repeat" | "repeat_colored")) => {
                        let Expression::Literal(Literal::Natural(rounds)) = &arguments[0].value
                        else {
                            return Err(Error("schedule repeat requires a literal count".into()));
                        };
                        Ok(Schedule::repeat(
                            rounds.parse::<usize>().map_err(error)?,
                            run,
                        ))
                    }
                    _ => Ok(Schedule::saturate(run)),
                }
            }
            (["std", "schedule", "sequence"], [items]) => {
                let children = parts(items)
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        self.schedule(
                            item,
                            &format!("@cdsl::{name}::{index}"),
                            modules,
                            definitions,
                            active,
                            rules,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Schedule::seq(children))
            }
            (["std", "schedule", "seq"], [first, second])
            | (["std", "schedule", "Schedule", "Sequence"], [first, second]) => {
                let first = self.schedule(
                    first,
                    &format!("@cdsl::{name}::0"),
                    modules,
                    definitions,
                    active,
                    rules,
                )?;
                let second = self.schedule(
                    second,
                    &format!("@cdsl::{name}::1"),
                    modules,
                    definitions,
                    active,
                    rules,
                )?;
                Ok(Schedule::seq([first, second]))
            }
            _ => Err(Error(format!("unsupported executable schedule {name}"))),
        }
    }
}

struct RuleLowering<'a> {
    schema: &'a Schema,
    operations: &'a BTreeMap<Path, (String, &'a Binding)>,
    declarations: &'a BTreeMap<Path, &'a Declaration>,
    binders: &'a [RuleBinder],
    operation_matches: Option<&'a BTreeMap<usize, CheckedMatch>>,
}

fn match_variable(index: usize) -> ast::Expr {
    ast::Expr::Var(tir_egglog::span!(), format!("@port:{index}"))
}

fn match_index(
    facts: &mut Vec<ast::Fact>,
    value: ast::Expr,
    expected: &PortIndex,
    fresh: &impl Fn(&str) -> ast::Expr,
    label: &str,
) {
    use tir_egglog::prelude::exprs::{call, int, string};
    let span = tir_egglog::span!();
    let expected = match expected {
        PortIndex::Variable(index) => match_variable(*index),
        PortIndex::Constant(constant) => int(*constant),
        PortIndex::Symbol(symbol) => {
            let id = fresh(&format!("{label}:symbol"));
            facts.push(ast::Fact::Fact(call(
                "Symbol",
                vec![
                    id.clone(),
                    string(symbol),
                    fresh(&format!("{label}:entity")),
                    fresh(&format!("{label}:kind")),
                ],
            )));
            id
        }
    };
    facts.push(ast::Fact::Eq(span, value, expected));
}

fn ir_host_path(name: &str) -> Path {
    ["std", "ir", name].map(str::to_owned).to_vec()
}

fn match_type(
    schema: &Schema,
    facts: &mut Vec<ast::Fact>,
    ty: ast::Expr,
    expected: &PortType,
    fresh: &impl Fn(&str) -> ast::Expr,
    label: &str,
) -> Result<(), Error> {
    use tir_egglog::prelude::exprs::{call, int, string};
    let span = tir_egglog::span!();
    let arguments = |facts: &mut Vec<ast::Fact>, dialect, name| {
        let ctor = fresh(&format!("{label}:ctor"));
        let args = fresh(&format!("{label}:args"));
        facts.push(ast::Fact::Fact(call(
            "TypeTerm",
            vec![ty.clone(), ctor.clone(), args.clone()],
        )));
        facts.push(ast::Fact::Fact(call(
            "TypeCtor",
            vec![
                ctor,
                string(dialect),
                string(name),
                fresh(&format!("{label}:declaration")),
                fresh(&format!("{label}:default")),
            ],
        )));
        args
    };
    let projection = |args, slot| {
        call(
            &schema.name(&ir_host_path("type_argument")),
            vec![args, int(slot)],
        )
    };
    let match_child = |facts: &mut Vec<ast::Fact>, args, expected: &PortType, field| {
        let child = fresh(&format!("{label}:{field}:id"));
        facts.push(ast::Fact::Eq(span, projection(args, 0), child.clone()));
        match_type(
            schema,
            facts,
            child,
            expected,
            fresh,
            &format!("{label}:{field}"),
        )
    };
    match expected {
        PortType::Variable(index) => {
            facts.push(ast::Fact::Eq(span, ty, match_variable(*index)));
        }
        PortType::Integer(width) => {
            let args = arguments(facts, "builtin", "i");
            match_index(
                facts,
                call(&schema.name(&ir_host_path("integer_width")), vec![args]),
                width,
                fresh,
                label,
            );
        }
        PortType::Pointer(pointee) => {
            let args = arguments(facts, "ptr", "p");
            match_child(facts, args, pointee, "pointee")?;
        }
        PortType::Float(format) => {
            let args = arguments(facts, "builtin", "f");
            match_index(
                facts,
                call(&schema.name(&ir_host_path("fp_format")), vec![args]),
                format,
                fresh,
                label,
            );
        }
        PortType::Computation {
            format,
            rounding,
            contraction,
        } => {
            let args = arguments(facts, "fp", "computation");
            match_child(facts, args.clone(), &PortType::Float(*format), "storage")?;
            match_index(
                facts,
                projection(args.clone(), 1),
                rounding,
                fresh,
                &format!("{label}:rounding"),
            );
            match_index(
                facts,
                projection(args, 2),
                contraction,
                fresh,
                &format!("{label}:contraction"),
            );
        }
        PortType::Unsupported => {
            return Err(Error(format!(
                "cannot enforce the checked type of operation match port {label}"
            )));
        }
    }
    Ok(())
}

struct MatchPorts<'a> {
    node: &'a Expr,
    operands: &'a [Expr],
    results: &'a [Expr],
}

impl<'a> MatchPorts<'a> {
    fn parse(arguments: &'a [Expr]) -> Result<Self, Error> {
        let [node, operands, results] = arguments else {
            return Err(Error(
                "operation match requires a node, operands and results".into(),
            ));
        };
        let packed = |expr: &Expr| matches!(expr.value, Expression::Unit | Expression::Tuple(_));
        if !packed(operands) || !packed(results) {
            return Err(Error("operation match ports must be tuples".into()));
        }
        Ok(Self {
            node,
            operands: parts(operands),
            results: parts(results),
        })
    }
}

impl RuleLowering<'_> {
    fn local<'a>(&self, path: &'a Path) -> Option<&'a str> {
        let [name] = path.as_slice() else {
            return None;
        };
        self.binders
            .iter()
            .any(|binder| &binder.name == name)
            .then_some(name)
    }

    fn predicate_row<'a>(&self, expr: &'a Expr) -> Result<std::borrow::Cow<'a, Expr>, Error> {
        let Expression::Call { value, arguments } = &expr.value else {
            return Ok(std::borrow::Cow::Borrowed(expr));
        };
        let Some(Declaration::Let { binding, .. }) = path(value)
            .ok()
            .and_then(|name| self.declarations.get(name))
        else {
            return Ok(std::borrow::Cow::Borrowed(expr));
        };
        let Expression::Function(function) = &binding.value.value else {
            return Ok(std::borrow::Cow::Borrowed(expr));
        };
        let body = &function.body;
        if !body.bindings.is_empty()
            || !matches!(&body.value.value, Expression::Call { value, .. }
            if path(value).is_ok_and(|p| self.schema.database().schema().table_named(&self.schema.name(p)).is_some()))
        {
            return Err(Error(
                "a predicate view must select one relation row".into(),
            ));
        }
        let values = function
            .parameters
            .iter()
            .zip(arguments)
            .map(|(param, arg)| (vec![param.name.clone()], arg.clone()))
            .collect();
        crate::normalize::substitute(&body.value, &values).map(std::borrow::Cow::Owned)
    }

    fn operation(&self, expr: &Expr, member: &str) -> Option<&(String, &Binding)> {
        let Expression::Path(path) = &expr.value else {
            return None;
        };
        if self.local(path).is_some() {
            return None;
        }
        let operation = self.operations.get(path)?;
        if matches!(&operation.1.value.value, Expression::Record(fields)
            if fields.iter().any(|(field, _)| field == member))
        {
            return None;
        }
        Some(operation)
    }

    fn bind_pattern<'a>(&self, expr: &'a Expr, bound: &mut BTreeSet<&'a str>) {
        match &expr.value {
            Expression::Path(path) => bound.extend(self.local(path)),
            Expression::Tuple(fields) => {
                for field in fields {
                    self.bind_pattern(field, bound);
                }
            }
            Expression::Call { value, arguments }
                if matches!(&value.value, Expression::Path(path) if self.schema.constructors.contains(path))
                    || matches!(
                        ir_function(value),
                        Some(
                            "operation_key"
                                | "value_key"
                                | "raw_value_id"
                                | "raw_operation_id"
                                | "operation_named"
                        )
                    ) =>
            {
                for argument in arguments {
                    self.bind_pattern(argument, bound);
                }
            }
            _ => {}
        }
    }

    fn positive_bindings<'a>(&self, clauses: &[&'a Expr]) -> BTreeSet<&'a str> {
        let mut bound = BTreeSet::new();
        let is_table = |value: &Expr| {
            path(value).is_ok_and(|path| {
                self.schema
                    .database()
                    .schema()
                    .table_named(&self.schema.name(path))
                    .is_some()
            })
        };
        for clause in clauses {
            match &clause.value {
                Expression::Call { value, arguments }
                    if ir_function(value) == Some("match_kind") =>
                {
                    for argument in arguments.iter().skip(1) {
                        self.bind_pattern(argument, &mut bound);
                    }
                }
                Expression::Call { value, arguments }
                    if static_match(value).is_some() || is_table(value) =>
                {
                    for argument in arguments {
                        self.bind_pattern(argument, &mut bound);
                    }
                }
                Expression::RecordCall { value, fields, .. } if is_table(value) => {
                    for (_, field) in fields {
                        self.bind_pattern(field, &mut bound);
                    }
                }
                _ => {}
            }
        }
        bound
    }

    fn validate_inputs(&self, clauses: &[&Expr]) -> Result<(), Error> {
        let mut bound = None;
        for clause in clauses {
            crate::totality::visit(clause, &mut |expr| {
                let Expression::Call { value, arguments } = &expr.value else {
                    return Ok(());
                };
                let Some(
                    function @ ("operation_name" | "operation_kind" | "value_id" | "value_ref_key"
                    | "match_kind"),
                ) = ir_function(value)
                else {
                    return Ok(());
                };
                let Some(argument) = arguments.first() else {
                    return Ok(());
                };
                crate::totality::visit(argument, &mut |input| {
                    if let Expression::Path(path) = &input.value
                        && let Some(name) = self.local(path)
                        && !bound
                            .get_or_insert_with(|| self.positive_bindings(clauses))
                            .contains(name)
                    {
                        return Err(Error(format!(
                            "{function} requires {name} to be bound by a positive premise"
                        )));
                    }
                    Ok(())
                })
            })?;
        }
        Ok(())
    }

    fn term(&self, expr: &Expr) -> Result<ast::Expr, Error> {
        let span = tir_egglog::span!();
        Ok(match &expr.value {
            Expression::Path(path) if self.schema.constructors.contains(path) => {
                ast::Expr::Call(span, self.schema.name(path), vec![])
            }
            Expression::Path(path) => ast::Expr::Var(span, path_name(path)),
            Expression::Member { value, name }
                if matches!(name.as_str(), "kind" | "typed_kind") =>
            {
                let (name, _) = self.operation(value, name).ok_or_else(|| {
                    Error("ordinary member projections are unsupported in executable rules".into())
                })?;
                ast::Expr::Lit(span, ast::Literal::String(name.clone()))
            }
            Expression::Literal(literal) => ast::Expr::Lit(
                span,
                match literal {
                    Literal::Natural(n) => ast::Literal::Int(n.parse().map_err(error)?),
                    Literal::Bool(b) => ast::Literal::Bool(*b),
                    Literal::String(s) => ast::Literal::String(s.clone()),
                },
            ),
            Expression::Call { value, arguments }
                if matches!(
                    ir_function(value),
                    Some(
                        "operation_name"
                            | "operation_kind"
                            | "operation_named"
                            | "value_id"
                            | "value_ref_key"
                            | "operation_key"
                            | "value_key"
                            | "raw_value_id"
                            | "raw_operation_id"
                    )
                ) =>
            {
                let [argument] = arguments.as_slice() else {
                    return Err(Error("identity projection requires one argument".into()));
                };
                self.term(argument)?
            }
            Expression::Call { value, .. }
                if static_match(value).is_some() || ir_function(value) == Some("match_kind") =>
            {
                return Err(Error(
                    "operation matches require a positive rule premise".into(),
                ));
            }
            Expression::Call { value, arguments } => ast::Expr::Call(
                span,
                self.schema.name(path(value)?),
                arguments
                    .iter()
                    .map(|arg| self.term(arg))
                    .collect::<Result<_, _>>()?,
            ),
            Expression::RecordCall {
                value,
                fields,
                arguments,
            } if arguments.is_empty() => {
                let relation = path(value)?;
                let name = self.schema.name(relation);
                let columns =
                    self.schema.named_relations.get(&name).ok_or_else(|| {
                        Error(format!("{name} does not have named relation columns"))
                    })?;
                let values = columns
                    .iter()
                    .map(|column| {
                        let (_, field) = fields
                            .iter()
                            .find(|(field, _)| field == column)
                            .ok_or_else(|| Error(format!("{name} is missing column {column}")))?;
                        self.term(field)
                    })
                    .collect::<Result<_, _>>()?;
                ast::Expr::Call(span, name, values)
            }
            Expression::Binary { op, left, right } => {
                let name = match op {
                    BinaryOp::Add => "+",
                    BinaryOp::Subtract => "-",
                    BinaryOp::Multiply => "*",
                    BinaryOp::Divide => "/",
                    BinaryOp::Remainder => "%",
                    _ => return Err(Error("unsupported executable term operator".into())),
                };
                ast::Expr::Call(span, name.into(), vec![self.term(left)?, self.term(right)?])
            }
            _ => return Err(Error("unsupported executable term".into())),
        })
    }
    fn fact(&self, expr: &Expr, facts: &mut Vec<ast::Fact>) -> Result<(), Error> {
        let span = tir_egglog::span!();
        if let Expression::Call { value, arguments } = &expr.value {
            let checked = || {
                self.operation_matches
                    .and_then(|matches| matches.get(&expr.range.start))
                    .ok_or_else(|| Error("missing checked operation match ports".into()))
            };
            if let Some(operation) = static_match(value) {
                let (kind, _) = self
                    .operation(operation, "matches")
                    .ok_or_else(|| Error("matches requires a declared operation".into()))?;
                let kind = ast::Expr::Lit(span, ast::Literal::String(kind.clone()));
                return self.match_facts(kind, MatchPorts::parse(arguments)?, checked()?, facts);
            }
            if ir_function(value) == Some("match_kind") {
                let Some((kind, ports)) = arguments.split_first() else {
                    return Err(Error("match_kind requires an operation kind".into()));
                };
                return self.match_facts(
                    self.term(kind)?,
                    MatchPorts::parse(ports)?,
                    checked()?,
                    facts,
                );
            }
        }
        let fact = if let Expression::Unary {
            op: UnaryOp::Not,
            value,
        } = &expr.value
        {
            ast::Fact::Not(self.term(value)?)
        } else if let Expression::Binary {
            op: op @ (BinaryOp::Equal | BinaryOp::NotEqual),
            left,
            right,
        } = &expr.value
        {
            let left = self.term(left)?;
            let right = self.term(right)?;
            match op {
                BinaryOp::Equal => ast::Fact::Eq(span, left, right),
                _ => ast::Fact::Fact(ast::Expr::Call(span, "!=".into(), vec![left, right])),
            }
        } else if let Expression::Call { value, arguments } = &expr.value
            && let Expression::Path(function) = &value.value
            && self.schema.functions.contains(function)
        {
            let (last, keys) = arguments
                .split_last()
                .ok_or_else(|| Error("function row requires output".into()))?;
            ast::Fact::Eq(
                span,
                ast::Expr::Call(
                    span,
                    self.schema.name(function),
                    keys.iter()
                        .map(|arg| self.term(arg))
                        .collect::<Result<_, _>>()?,
                ),
                self.term(last)?,
            )
        } else {
            ast::Fact::Fact(self.term(expr)?)
        };
        facts.push(fact);
        Ok(())
    }

    fn match_facts(
        &self,
        kind: ast::Expr,
        ports: MatchPorts<'_>,
        checked: &CheckedMatch,
        facts: &mut Vec<ast::Fact>,
    ) -> Result<(), Error> {
        if ports.operands.len() != checked.operands.len()
            || ports.results.len() != checked.results.len()
        {
            return Err(Error(
                "operation match port count disagrees with checked types".into(),
            ));
        }
        let span = tir_egglog::span!();
        let node = self.term(ports.node)?;
        let match_index = facts.len();
        let fresh = |column: &str| ast::Expr::Var(span, format!("@match:{match_index}:{column}"));
        let string = |s: &str| ast::Expr::Lit(span, ast::Literal::String(s.into()));
        let index = |slot: usize| ast::Expr::Lit(span, ast::Literal::Int(slot as i64));
        let columns = self
            .schema
            .named_relations
            .get("Operation")
            .ok_or_else(|| Error("operation matches require the named Operation schema".into()))?;
        facts.push(ast::Fact::Fact(ast::Expr::Call(
            span,
            "Operation".into(),
            columns
                .iter()
                .map(|column| match column.as_str() {
                    "id" => node.clone(),
                    "kind" => kind.clone(),
                    column => fresh(column),
                })
                .collect(),
        )));
        for (slot, (operand, expected)) in ports.operands.iter().zip(&checked.operands).enumerate()
        {
            let value = self.term(operand)?;
            facts.push(ast::Fact::Eq(
                span,
                ast::Expr::Call(span, "Operand".into(), vec![node.clone(), index(slot)]),
                value.clone(),
            ));
            let port = format!("operand:{slot}");
            let label = |name: &str| fresh(&format!("{port}:{name}"));
            let ty = label("type");
            facts.push(ast::Fact::Fact(ast::Expr::Call(
                span,
                "Value".into(),
                vec![
                    value,
                    ty.clone(),
                    label("kind"),
                    label("owner"),
                    label("slot"),
                    label("declared"),
                ],
            )));
            match_type(self.schema, facts, ty, expected, &fresh, &port)?;
        }
        for (slot, (result, expected)) in ports.results.iter().zip(&checked.results).enumerate() {
            let port = format!("result:{slot}");
            let label = |name: &str| fresh(&format!("{port}:{name}"));
            let ty = label("type");
            facts.push(ast::Fact::Fact(ast::Expr::Call(
                span,
                "Value".into(),
                vec![
                    self.term(result)?,
                    ty.clone(),
                    string("result"),
                    node.clone(),
                    index(slot),
                    label("declared"),
                ],
            )));
            match_type(self.schema, facts, ty, expected, &fresh, &port)?;
        }
        Ok(())
    }

    fn action(&self, expr: &Expr) -> Result<ast::Action, Error> {
        let span = tir_egglog::span!();
        if let Expression::Binary {
            op: BinaryOp::Equal,
            left,
            right,
        } = &expr.value
        {
            return Ok(ast::Action::Union(
                span,
                self.term(left)?,
                self.term(right)?,
            ));
        }
        if let Expression::Call { value, arguments } = &expr.value
            && let Expression::Path(function) = &value.value
            && self.schema.functions.contains(function)
        {
            let (last, keys) = arguments
                .split_last()
                .ok_or_else(|| Error("function row requires output".into()))?;
            return Ok(ast::Action::Set(
                span,
                self.schema.name(function),
                keys.iter()
                    .map(|arg| self.term(arg))
                    .collect::<Result<_, _>>()?,
                self.term(last)?,
            ));
        }
        Ok(ast::Action::Expr(span, self.term(expr)?))
    }
}

/// Typed rules ready for engine planning against the installed schema.
pub struct LoweredProgram {
    rules: Rules,
    schedules: BTreeMap<Path, Schedule>,
}
impl LoweredProgram {
    /// Checked source schedules, with references bound to this program's rulesets.
    pub fn schedules(&self) -> &BTreeMap<Path, Schedule> {
        &self.schedules
    }
    /// Plan executable instructions against a database without replacing it.
    pub fn compile(&self, database: &Database) -> Result<tir_egglog::Program, Error> {
        self.rules.compile(database).map_err(error)
    }
}
