//! Executable lowering against an explicitly shared engine schema.
mod operation;
mod program;
mod rewrite;

use program::{Dependencies, Selection};

use std::collections::{BTreeMap, BTreeSet};

use tir_egglog::{Database, EGraph, Merge, Rules, Schedule, SortId, SortKind, TableKind, ast};

use crate::{
    CheckedProgram, Error,
    ast::{
        BinaryOp, Binding, Block, Declaration, Expr, Expression, Function, Literal, Path,
        RuleBinder, UnaryOp,
    },
    kind::OperationImport,
    lower::{CheckedMatch, PortIndex, PortType},
};

/// Reusable schema and host bindings, shared by independently compiled programs.
#[derive(Clone)]
pub struct Schema {
    engine: EGraph,
    hosts: BTreeSet<Path>,
    functions: BTreeSet<Path>,
    constructors: BTreeSet<Path>,
    named_relations: BTreeMap<String, Vec<String>>,
    local_types: BTreeMap<Path, Expr>,
    operation_imports: BTreeMap<Path, OperationImport>,
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
        name.as_str(), "operation_kind" |
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
pub(crate) fn table_type(ty: &Expr) -> Option<&[Expr]> {
    match &ty.value {
        Expression::Generic { value, arguments }
            if path(value).is_ok_and(|path| path == &["std", "table", "Table"]) =>
        {
            Some(arguments)
        }
        _ => None,
    }
}
fn table_parameter(ty: &Expr) -> bool {
    table_type(ty).is_some() || arrow(ty).is_some_and(|(_, output)| proposition(output))
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
            local_types: BTreeMap::new(),
            operation_imports: BTreeMap::new(),
        }
    }

    /// Install each relation and table under its declared name.
    pub fn new(program: &CheckedProgram) -> Result<Self, Error> {
        let mut schema = Self::from_engine(EGraph::default());
        let dependencies = Dependencies::collect(program, &[], Selection::FullSchema);
        schema.install(&dependencies, true)?;
        schema.bind_operation_imports(dependencies.operation_imports(program))?;
        Ok(schema)
    }

    /// Check a program against an existing engine, including its native functions.
    pub fn with_engine(program: &CheckedProgram, engine: EGraph) -> Result<Self, Error> {
        let mut schema = Self::from_engine(engine);
        schema.install(
            &Dependencies::collect(program, &[], Selection::Program),
            false,
        )?;
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
            .map(|path| (path, dependencies.declarations[path]));
        if dependencies
            .needed
            .iter()
            .any(|path| path == &["std", "table", "IntSet"])
            && self.database().schema().sort_named("IntSet").is_none()
        {
            let span = tir_egglog::span!();
            let set = Some(("Set".into(), vec![ast::Expr::Var(span, "i64".into())]));
            self.engine
                .declare_sort("IntSet", &set, span)
                .map_err(error)?;
        }
        let declarations =
            declarations.filter(|(path, _)| path.first().is_none_or(|part| part != "std"));
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
                self.install_keyed_table(full.clone(), arguments)?;
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

    fn install_keyed_table(&mut self, full: Path, arguments: &[Expr]) -> Result<(), Error> {
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
            Some(kind @ ("Min" | "Max" | "SetUnion")) => {
                let name = match kind {
                    "Min" => "min",
                    "Max" => "max",
                    _ => "set-union",
                };
                let schema = self.database().schema();
                let prim = schema
                    .primitives(name)
                    .find(|&id| {
                        let sig = schema.primitive(id).1;
                        sig.inputs == [out, out] && sig.output == out
                    })
                    .ok_or_else(|| Error(format!("invalid output sort for {kind} merge")))?;
                Merge::Prim(prim, vec![Merge::Old, Merge::New])
            }
            _ => return Err(Error("unsupported executable table merge".into())),
        };
        let name = self.name(&full);
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
        self.functions.insert(full);
        Ok(())
    }

    fn install_local_tables(&mut self, dependencies: &Dependencies<'_>) -> Result<(), Error> {
        for owner in &dependencies.ordered {
            let Declaration::Let { binding, .. } = dependencies.declarations[owner] else {
                continue;
            };
            crate::totality::visit(&binding.value, &mut |expression| {
                let Expression::LocalTable(ty) = &expression.value else {
                    return Ok(());
                };
                let name = local_table_path(owner, expression.range.start);
                if let Some(args) = table_type(ty) {
                    self.install_keyed_table(name.clone(), args)?;
                } else if let Some((args, result)) = arrow(ty)
                    && proposition(result)
                {
                    self.install_relation(name.clone(), args)?;
                } else {
                    return Err(Error(
                        "local table requires a relation or Table schema".into(),
                    ));
                }
                self.local_types.insert(name, (**ty).clone());
                Ok(())
            })?;
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

    fn bind_operation_imports(
        &mut self,
        imports: BTreeMap<Path, OperationImport>,
    ) -> Result<(), Error> {
        let output = self.database().sort("NativeIdentity").map_err(error)?;
        for import in imports.values() {
            self.engine.database_mut().add_primitive(
                &import.symbol,
                tir_egglog::Signature {
                    inputs: vec![],
                    output,
                },
                |_, _| None,
            );
        }
        self.operation_imports = imports;
        Ok(())
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
                "NativeIdentity".into()
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
                return self.database().sort("NativeIdentity").map_err(error);
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
            false,
        )
    }

    /// Run a named, nullary schedule with function-local tables.
    /// A script consumes its effects and releases any returned tables.
    pub fn run_local_schedule(
        &self,
        program: &CheckedProgram,
        name: &[String],
        db: &mut Database,
    ) -> Result<Option<tir_egglog::RunReport>, Error> {
        let dependencies = Dependencies::collect(program, &[], Selection::Program);
        let Some(Declaration::Let { binding, .. }) = dependencies.declarations.get(name) else {
            return Ok(None);
        };
        let Expression::Function(function) = &binding.value.value else {
            return Ok(None);
        };
        if !function.has_frame() {
            return Ok(None);
        }
        if !function.parameters.is_empty() {
            return Err(Error("a script schedule must take no arguments".into()));
        }
        let mut compiler = self.clone();
        compiler.install_local_tables(&dependencies)?;
        let lowered = compiler.lower_dependencies(program, &dependencies, true)?;
        let frame = lowered
            .scoped
            .get(name)
            .ok_or_else(|| Error("function does not have an executable schedule".into()))?;
        let templates = compiler.table_templates(&dependencies, &name[..name.len() - 1]);
        let compiled = frame.compile(compiler.database(), &lowered.rules, &templates)?;
        let locals = compiled
            .locals
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let result = (|| {
            let (mut code, _locals) =
                tir_egglog::Program::from_bytes_with_local_tables(&compiled.bytes, db, &locals)
                    .map_err(error)?;
            code.run_in(db, tir_egglog::Color::BASE, &frame.schedule, &[])
                .map_err(error)
        })();
        db.collect_local_tables();
        result.map(Some)
    }

    fn table_templates(
        &self,
        dependencies: &Dependencies<'_>,
        selected: &[String],
    ) -> BTreeSet<String> {
        dependencies
            .ordered
            .iter()
            .filter(|path| path.starts_with(selected))
            .filter_map(|path| storage_name(dependencies.declarations[path]))
            .map(str::to_owned)
            .collect()
    }

    fn lower_dependencies(
        &self,
        program: &CheckedProgram,
        dependencies: &Dependencies<'_>,
        include_frames: bool,
    ) -> Result<LoweredProgram, Error> {
        let operations = dependencies.operation_imports(program);
        let mut rules = Rules::new();
        let mut members = BTreeMap::new();
        let mut definitions = BTreeMap::new();
        for full in &dependencies.ordered {
            let module = &full[..full.len() - 1];
            if module.first().is_some_and(|name| name == "std") {
                continue;
            }
            let ruleset = path_name(full);
            let declaration = dependencies.declarations[full];
            if let Declaration::Let { binding, .. } = declaration {
                if dependencies.operations.contains_key(full) {
                    continue;
                }
                if let Some(arguments) = operation::integer_rule_arguments(binding) {
                    let names = ["flags", "fold"].map(|name| {
                        let path = [full.as_slice(), &[name.to_owned()]].concat();
                        let name = path_name(&path);
                        members.insert(path, name.clone());
                        name
                    });
                    self.add_integer_operations(
                        program, arguments, &mut rules, &names[0], &names[1],
                    )?;
                    continue;
                }
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
            members.insert(full.clone(), ruleset.clone());
            RuleLowering {
                schema: self,
                operations: &operations,
                declarations: &dependencies.declarations,
                binders: generics,
                operation_matches: program.operation_matches.get(full),
                captures: &BTreeMap::new(),
                tables: &BTreeMap::new(),
            }
            .install(&mut rules, &ruleset, ty, None)
            .map_err(|error| Error(format!("{ruleset}::{name}: {error}")))?;
        }
        let mut schedules = BTreeMap::new();
        let mut scoped = BTreeMap::new();
        let mut collections = BTreeMap::new();
        let values = dependencies
            .declarations
            .iter()
            .filter_map(|(path, declaration)| {
                let Declaration::Let { binding, .. } = declaration else {
                    return None;
                };
                (!rewrite::carries_clause(&binding.value)).then_some((path.clone(), binding))
            })
            .collect::<BTreeMap<_, _>>();
        let constructors = dependencies
            .declarations
            .iter()
            .flat_map(|(path, declaration)| {
                let mut names = Vec::new();
                if let Declaration::Enum { variants, .. } = declaration {
                    names.extend(variants.iter().map(|variant| {
                        [path.as_slice(), std::slice::from_ref(&variant.name)].concat()
                    }));
                }
                names
            })
            .collect::<BTreeSet<_>>();
        let schedule_type = program
            .type_families
            .get(["std".into(), "schedule".into(), "Schedule".into()].as_slice())
            .copied();
        let collection_type = program
            .type_families
            .get(["std".into(), "rewrite".into(), "Collection".into()].as_slice())
            .copied();
        let enum_types = dependencies
            .declarations
            .iter()
            .filter(|(_, declaration)| matches!(declaration, Declaration::Enum { .. }))
            .filter_map(|(path, _)| program.type_families.get(path).copied())
            .collect::<Vec<_>>();
        let mut lowering = ScheduleLowering {
            schema: self,
            program,
            dependencies,
            operations: &operations,
            definitions: &definitions,
            members: &members,
            constructors: &constructors,
            active: BTreeSet::new(),
            global: BTreeMap::new(),
            bundles: BTreeMap::new(),
            next_bundle: 0,
            rules: &mut rules,
        };
        for (path, expression) in &definitions {
            let crate::hm::Ty::Fun(_, result) = &program.types()[path].ty else {
                continue;
            };
            let is_schedule = matches!(&**result, crate::hm::Ty::Con(family, _) if schedule_type.as_ref() == Some(family));
            let name = path_name(path);
            if let Expression::Function(function) = &expression.value
                && matches!(&**result, crate::hm::Ty::Con(family, _) if collection_type.as_ref() == Some(family))
            {
                if !function.parameters.is_empty() || !function.generics.is_empty() {
                    return Err(Error(format!(
                        "rule collection {name} requires a nullary function"
                    )));
                }
                let body = Expr {
                    value: Expression::Block(function.body.clone()),
                    range: expression.range.clone(),
                };
                let value = crate::normalize::head(&body, &values, &constructors)?;
                let (value, policy) = rewrite::collection_arguments(&value)?;
                let policy = rewrite::Policy::resolve(self, policy, &values, &constructors)?;
                let mut members = Vec::new();
                let mut visit = |owner: &Path,
                                 binders: &[RuleBinder],
                                 clause: &Expr|
                 -> Result<(), Error> {
                    let label = format!("@cdsl::{name}::collection::{}", members.len());
                    RuleLowering {
                        schema: self,
                        operations: &operations,
                        declarations: &dependencies.declarations,
                        binders,
                        operation_matches: program.operation_matches.get(owner),
                        captures: &BTreeMap::new(),
                        tables: &BTreeMap::new(),
                    }
                    .install(lowering.rules, &label, clause, Some(&policy))?;
                    members.push(label);
                    Ok(())
                };
                rewrite::visit_rules(value, path, &definitions, &constructors, &mut visit)?;
                lowering
                    .rules
                    .add_combined_ruleset(&name, &members)
                    .map_err(error)?;
                collections.insert(path.clone(), name);
                continue;
            }
            if !is_schedule
                && matches!(&**result, crate::hm::Ty::Con(family, _) if enum_types.contains(family))
            {
                continue;
            }
            lowering.active.insert(path.clone());
            let Expression::Function(function) = &expression.value else {
                return Err(Error(format!("unsupported executable definition {name}")));
            };
            if function
                .parameters
                .iter()
                .any(|parameter| table_parameter(&parameter.ty))
            {
                lowering.active.remove(path);
                continue;
            }
            let args = function
                .parameters
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    Ok(ScheduleArgument::Value(tir_egglog::prelude::exprs::arg(
                        u32::try_from(index).map_err(error)?,
                    )))
                })
                .collect::<Result<Vec<_>, Error>>()?;
            if function.has_frame() {
                if include_frames {
                    let frame = lowering.scoped(function, args, &name, path, is_schedule)?;
                    scoped.insert(path.clone(), frame);
                }
            } else {
                if !is_schedule {
                    return Err(Error(format!("unsupported executable definition {name}")));
                }
                schedules.insert(
                    path.clone(),
                    lowering.call_schedule(function, args, &name, path)?,
                );
            }
            lowering.active.remove(path);
        }
        Ok(LoweredProgram {
            rules,
            schedules,
            scoped,
            collections,
        })
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
}

impl Function {
    fn has_frame(&self) -> bool {
        self.memo
            || self.body.bindings.iter().any(|binding| {
                matches!(
                    binding.value.value,
                    Expression::LocalTable(_) | Expression::Run(_)
                )
            })
    }
}

fn local_table_path(owner: &[String], position: usize) -> Path {
    vec![format!("@local::{}::{position}", path_name(owner))]
}

enum ScheduleArgument {
    Value(ast::Expr),
    Table(Path),
}

struct ScheduleScope<'a> {
    owner: &'a Path,
    captures: BTreeMap<Path, ast::Expr>,
    rules: BTreeMap<Path, Vec<String>>,
    tables: BTreeMap<Path, Path>,
    values: BTreeMap<Path, Expr>,
}

struct ScheduleLowering<'a, 'program> {
    schema: &'a Schema,
    program: &'program CheckedProgram,
    dependencies: &'a Dependencies<'program>,
    operations: &'a BTreeMap<Path, OperationImport>,
    definitions: &'a BTreeMap<Path, Expr>,
    members: &'a BTreeMap<Path, String>,
    constructors: &'a BTreeSet<Path>,
    active: BTreeSet<Path>,
    global: BTreeMap<Path, Schedule>,
    bundles: BTreeMap<Path, Vec<String>>,
    next_bundle: usize,
    rules: &'a mut Rules,
}

impl ScheduleLowering<'_, '_> {
    fn argument_scope<'a>(
        function: &Function,
        arguments: Vec<ScheduleArgument>,
        owner: &'a Path,
    ) -> Result<ScheduleScope<'a>, Error> {
        if function.parameters.len() != arguments.len() || !function.generics.is_empty() {
            return Err(Error(format!(
                "unsupported executable signature for {}",
                path_name(owner)
            )));
        }
        let mut captures = BTreeMap::new();
        let mut tables = BTreeMap::new();
        for (parameter, argument) in function.parameters.iter().zip(arguments) {
            let name = vec![parameter.name.clone()];
            match argument {
                ScheduleArgument::Value(value) => {
                    captures.insert(name, value);
                }
                ScheduleArgument::Table(table) => {
                    tables.insert(name, table);
                }
            }
        }
        Ok(ScheduleScope {
            owner,
            captures,
            tables,
            rules: BTreeMap::new(),
            values: BTreeMap::new(),
        })
    }

    fn call_arguments(
        &self,
        function: &Function,
        arguments: &[Expr],
        scope: &ScheduleScope<'_>,
    ) -> Result<Vec<ScheduleArgument>, Error> {
        if function.parameters.len() != arguments.len() {
            return Err(Error("wrong number of executable arguments".into()));
        }
        let lowering = RuleLowering {
            schema: self.schema,
            operations: self.operations,
            declarations: &self.dependencies.declarations,
            binders: &[],
            operation_matches: None,
            captures: &scope.captures,
            tables: &scope.tables,
        };
        function
            .parameters
            .iter()
            .zip(arguments)
            .map(|(parameter, argument)| {
                if table_parameter(&parameter.ty) {
                    let path = path(argument)?;
                    let table = scope.tables.get(path).unwrap_or(path);
                    self.schema
                        .database()
                        .table(&self.schema.name(table))
                        .map_err(error)?;
                    Ok(ScheduleArgument::Table(table.clone()))
                } else {
                    Ok(ScheduleArgument::Value(lowering.term(argument)?))
                }
            })
            .collect()
    }

    fn scoped(
        &mut self,
        function: &Function,
        args: Vec<ScheduleArgument>,
        name: &str,
        owner: &Path,
        is_schedule: bool,
    ) -> Result<ScopedFunction, Error> {
        // Root declarations and lexical bindings can have the same source path.
        let renamed = crate::normalize::substitute(
            &Expr {
                value: Expression::Function(Box::new(function.clone())),
                range: function.body.value.range.clone(),
            },
            &BTreeMap::new(),
        )?;
        let Expression::Function(function) = renamed.value else {
            return Err(Error("expected a renamed scoped function".into()));
        };
        let function = function.as_ref();
        let scope = Self::argument_scope(function, args, owner)?;
        let (scope, mut effects) = self.bind_block(&function.body, name, scope)?;
        let mut fields = Vec::new();
        let mut query = None;
        let mut value = None;
        let table = |expr: &Expr| -> Result<Path, Error> {
            let path = path(expr)?;
            Ok(scope.tables.get(path).unwrap_or(path).clone())
        };
        let result = if scope.values.is_empty() {
            (*function.body.value).clone()
        } else {
            crate::normalize::substitute(&function.body.value, &scope.values)?
        };
        match &result.value {
            Expression::Function(closure) => {
                let body = Expr {
                    value: Expression::Block(closure.body.clone()),
                    range: result.range.clone(),
                };
                let body = crate::normalize::head(&body, &BTreeMap::new(), &BTreeSet::new())?;
                let mut tables = BTreeMap::new();
                let mut captured = BTreeSet::new();
                crate::totality::visit(&body, &mut |expr| {
                    let Expression::Path(alias) = &expr.value else {
                        return Ok(());
                    };
                    let table = scope.tables.get(alias).or_else(|| {
                        self.dependencies
                            .declarations
                            .get(alias)
                            .and_then(|declaration| storage_name(declaration))
                            .map(|_| alias)
                    });
                    if let Some(table) = table {
                        tables.insert(alias.clone(), table.clone());
                        if captured.insert(table.clone()) {
                            let field = if fields.is_empty() {
                                "rows".into()
                            } else {
                                format!("rows_{}", fields.len() + 1)
                            };
                            fields.push((field, table.clone()));
                        }
                    }
                    Ok(())
                })?;
                if tables.is_empty() {
                    return Err(Error("an escaping query must capture a table".into()));
                }
                query = Some(ScopedQuery {
                    parameters: closure.parameters.clone(),
                    body,
                    tables,
                });
            }
            Expression::Record(outputs)
                if outputs.iter().all(|(_, value)| {
                    table(value)
                        .is_ok_and(|p| self.schema.database().table(&self.schema.name(&p)).is_ok())
                }) =>
            {
                for (name, expr) in outputs {
                    fields.push((name.clone(), table(expr)?));
                }
            }
            Expression::Unit => {}
            _ if is_schedule => effects.push(self.schedule(&function.body.value, name, &scope)?),
            _ => value = Some(result),
        }
        let locals = scope
            .tables
            .values()
            .map(|path| self.schema.name(path))
            .collect();
        Ok(ScopedFunction {
            parameters: function.parameters.clone(),
            schedule: Schedule::seq(effects),
            locals,
            fields,
            query,
            value,
            memo: function.memo,
        })
    }

    fn bundle(
        &mut self,
        expression: &Expr,
        scope: &ScheduleScope<'_>,
    ) -> Result<Vec<String>, Error> {
        match &expression.value {
            Expression::Path(path) => {
                if let Some(names) = scope.rules.get(path) {
                    return Ok(names.clone());
                }
                if let Some(value) = scope.values.get(path) {
                    return self.bundle(value, scope);
                }
                if let Some(name) = self.members.get(path) {
                    return Ok(vec![name.clone()]);
                }
                if let Some(names) = self.bundles.get(path) {
                    return Ok(names.clone());
                }
                if self.constructors.contains(path) {
                    return Ok(Vec::new());
                }
                let definition = self.definitions.get(path).ok_or_else(|| {
                    Error(format!(
                        "{} is not an executable rule value",
                        path_name(path)
                    ))
                })?;
                if !self.active.insert(path.clone()) {
                    return Err(Error("cyclic executable rule bundle".into()));
                }
                let global = ScheduleScope {
                    owner: path,
                    captures: BTreeMap::new(),
                    rules: BTreeMap::new(),
                    values: BTreeMap::new(),
                    tables: BTreeMap::new(),
                };
                let result = if let Expression::Block(block) = &definition.value {
                    self.bind_block(block, &path_name(path), global)
                        .and_then(|(scope, _)| self.bundle(&block.value, &scope))
                } else {
                    self.bundle(definition, &global)
                };
                self.active.remove(path);
                let names = result?;
                self.bundles.insert(path.clone(), names.clone());
                Ok(names)
            }
            Expression::Member { value, name } => {
                let target = [path(value)?.as_slice(), std::slice::from_ref(name)].concat();
                self.members
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
            Expression::Call { value, arguments } if self.is_constructor(value) => {
                self.bundle_items(arguments, scope)
            }
            Expression::Call { value, arguments } => {
                let owner = path(value)?;
                let Some(Expr {
                    value: Expression::Function(function),
                    ..
                }) = self.definitions.get(owner)
                else {
                    return Err(Error(format!(
                        "{} is not a rule function",
                        path_name(owner)
                    )));
                };
                if function.has_frame() {
                    return Err(Error("a rule function cannot execute a schedule".into()));
                }
                if !self.active.insert(owner.clone()) {
                    return Err(Error("cyclic executable rule function".into()));
                }
                let args = self.call_arguments(function, arguments, scope)?;
                let scope = Self::argument_scope(function, args, owner)?;
                let name = format!("{}::bundle::{}", path_name(owner), self.next_bundle);
                self.next_bundle += 1;
                let (scope, _) = self.bind_block(&function.body, &name, scope)?;
                let result = self.bundle(&function.body.value, &scope);
                self.active.remove(owner);
                result
            }
            Expression::Generic { .. } if self.is_constructor(expression) => Ok(Vec::new()),
            Expression::Tuple(items) => self.bundle_items(items, scope),
            Expression::Unit => Ok(Vec::new()),
            _ => Err(Error(
                "expected an executable rule value or tuple of rules".into(),
            )),
        }
    }

    fn is_constructor(&self, expression: &Expr) -> bool {
        let value = match &expression.value {
            Expression::Generic { value, .. } => value,
            _ => expression,
        };
        path(value).is_ok_and(|path| self.constructors.contains(path))
    }

    fn bundle_items(
        &mut self,
        items: &[Expr],
        scope: &ScheduleScope<'_>,
    ) -> Result<Vec<String>, Error> {
        let mut names = Vec::new();
        for item in items {
            names.extend(self.bundle(item, scope)?);
        }
        Ok(names)
    }

    fn call_schedule(
        &mut self,
        function: &Function,
        args: Vec<ScheduleArgument>,
        name: &str,
        owner: &Path,
    ) -> Result<Schedule, Error> {
        if !function.parameters.is_empty() {
            return self.function(function, args, name, owner);
        }
        if let Some(schedule) = self.global.get(owner) {
            return Ok(schedule.clone());
        }
        let schedule = self.function(function, args, &path_name(owner), owner)?;
        self.global.insert(owner.clone(), schedule.clone());
        Ok(schedule)
    }

    fn function(
        &mut self,
        function: &Function,
        args: Vec<ScheduleArgument>,
        name: &str,
        owner: &Path,
    ) -> Result<Schedule, Error> {
        let scope = Self::argument_scope(function, args, owner)?;
        self.block(&function.body, name, scope)
    }

    fn block(
        &mut self,
        block: &Block,
        name: &str,
        scope: ScheduleScope<'_>,
    ) -> Result<Schedule, Error> {
        let (scope, mut effects) = self.bind_block(block, name, scope)?;
        effects.push(self.schedule(&block.value, name, &scope)?);
        if let [only] = effects.as_slice() {
            return Ok(only.clone());
        }
        Ok(Schedule::seq(effects))
    }

    fn bind_block<'a>(
        &mut self,
        block: &Block,
        name: &str,
        mut scope: ScheduleScope<'a>,
    ) -> Result<(ScheduleScope<'a>, Vec<Schedule>), Error> {
        let mut effects = Vec::new();
        for (index, binding) in block.bindings.iter().enumerate() {
            match &binding.value.value {
                Expression::LocalTable(_) => {
                    scope.tables.insert(
                        binding.path.clone(),
                        local_table_path(scope.owner, binding.value.range.start),
                    );
                    continue;
                }
                Expression::Run(schedule) => {
                    let label = format!("@cdsl::{name}::run::{index}");
                    effects.push(self.schedule(schedule, &label, &scope)?);
                    continue;
                }
                _ => {}
            }
            if let Expression::Rule { binders, clause } = &binding.value.value {
                let label = format!("@cdsl::{name}::rule::{index}");
                RuleLowering {
                    schema: self.schema,
                    operations: self.operations,
                    declarations: &self.dependencies.declarations,
                    binders,
                    operation_matches: self.program.operation_matches.get(scope.owner),
                    captures: &scope.captures,
                    tables: &scope.tables,
                }
                .install(self.rules, &label, clause, None)?;
                scope.values.remove(&binding.path);
                scope.rules.insert(binding.path.clone(), vec![label]);
            } else {
                let value = crate::normalize::substitute(&binding.value, &scope.values)?;
                let lowering = RuleLowering {
                    schema: self.schema,
                    operations: self.operations,
                    declarations: &self.dependencies.declarations,
                    binders: &[],
                    operation_matches: None,
                    captures: &scope.captures,
                    tables: &scope.tables,
                };
                let term = lowering.term(&value).ok();
                scope.captures.remove(&binding.path);
                if let Some(term) = term {
                    scope.captures.insert(binding.path.clone(), term);
                }
                scope.rules.remove(&binding.path);
                scope.values.insert(binding.path.clone(), value);
            }
        }
        Ok((scope, effects))
    }

    fn schedule(
        &mut self,
        expression: &Expr,
        name: &str,
        scope: &ScheduleScope<'_>,
    ) -> Result<Schedule, Error> {
        if let Expression::Path(path) = &expression.value {
            if let Some(value) = scope.values.get(path) {
                return self.schedule(value, name, scope);
            }
            if let Some(schedule) = self.global.get(path) {
                return Ok(schedule.clone());
            }
            if !self.active.insert(path.clone()) {
                return Err(Error("cyclic executable schedule".into()));
            }
            let expression = self.definitions.get(path).ok_or_else(|| {
                Error(format!("{} is not a schedule definition", path_name(path)))
            })?;
            let global = ScheduleScope {
                owner: path,
                captures: BTreeMap::new(),
                rules: BTreeMap::new(),
                values: BTreeMap::new(),
                tables: BTreeMap::new(),
            };
            let result = self.schedule(expression, &path_name(path), &global);
            self.active.remove(path);
            let result = result?;
            self.global.insert(path.clone(), result.clone());
            return Ok(result);
        }
        if let Expression::Block(block) = &expression.value {
            return self.block(
                block,
                name,
                ScheduleScope {
                    owner: scope.owner,
                    captures: scope.captures.clone(),
                    rules: scope.rules.clone(),
                    values: scope.values.clone(),
                    tables: scope.tables.clone(),
                },
            );
        }
        let Expression::Call { value, arguments } = &expression.value else {
            return Err(Error(format!("unsupported executable definition {name}")));
        };
        let path = path(value)?;
        let callee = path.iter().map(String::as_str).collect::<Vec<_>>();
        match (callee.as_slice(), arguments.as_slice()) {
            (["std", "schedule", "Schedule", "Rules"], [bundle])
            | (["std", "schedule", "Schedule", "Repeat"], [_, bundle])
            | (["std", "schedule", "Schedule", "Saturate"], [bundle])
            | (["std", "schedule", "saturate"], [bundle])
            | (["std", "schedule", "repeat_colored"], [_, bundle]) => {
                let mut seen = BTreeSet::new();
                let members = self
                    .bundle(bundle, scope)?
                    .into_iter()
                    .filter(|member| seen.insert(member.clone()))
                    .collect::<Vec<_>>();
                if callee.last() == Some(&"repeat_colored") {
                    for member in &members {
                        self.rules.set_colored(member);
                    }
                }
                self.rules
                    .add_combined_ruleset(name, &members)
                    .map_err(error)?;
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
            (["std", "schedule", "sequence"], [items]) => self.sequence(parts(items), name, scope),
            (
                ["std", "schedule", "seq"] | ["std", "schedule", "Schedule", "Sequence"],
                [first, second],
            ) => self.sequence([first, second], name, scope),
            _ => {
                let Some(Expr {
                    value: Expression::Function(function),
                    ..
                }) = self.definitions.get(path)
                else {
                    return Err(Error(format!("unsupported executable schedule {name}")));
                };
                if !self.active.insert(path.clone()) {
                    return Err(Error("cyclic executable schedule function".into()));
                }
                let args = self.call_arguments(function, arguments, scope)?;
                let result = self.call_schedule(function, args, name, path);
                self.active.remove(path);
                result
            }
        }
    }

    fn sequence<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a Expr>,
        name: &str,
        scope: &ScheduleScope<'_>,
    ) -> Result<Schedule, Error> {
        let children = items
            .into_iter()
            .enumerate()
            .map(|(index, item)| self.schedule(item, &format!("@cdsl::{name}::{index}"), scope))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Schedule::seq(children))
    }
}

struct RuleLowering<'a> {
    schema: &'a Schema,
    operations: &'a BTreeMap<Path, OperationImport>,
    declarations: &'a BTreeMap<Path, &'a Declaration>,
    binders: &'a [RuleBinder],
    captures: &'a BTreeMap<Path, ast::Expr>,
    tables: &'a BTreeMap<Path, Path>,
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
    fn table_path<'a>(&'a self, path: &'a Path) -> &'a Path {
        self.tables.get(path).unwrap_or(path)
    }

    fn name(&self, path: &Path) -> String {
        self.schema.name(self.table_path(path))
    }

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

    fn operation(&self, expr: &Expr, member: &str) -> Option<&OperationImport> {
        let Expression::Path(path) = &expr.value else {
            return None;
        };
        if self.local(path).is_some() {
            return None;
        }
        let operation = self.operations.get(path)?;
        let Declaration::Let { binding, .. } = self.declarations[path] else {
            return None;
        };
        if matches!(&binding.value.value, Expression::Record(fields)
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
                        Some("operation_key" | "value_key" | "raw_value_id" | "raw_operation_id")
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
                    .table_named(&self.name(path))
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

    fn install(
        &self,
        rules: &mut Rules,
        name: &str,
        clause: &Expr,
        policy: Option<&rewrite::Policy>,
    ) -> Result<(), Error> {
        let (premise, head) = arrow(clause).map_or((None, clause), |(a, b)| (Some(a), b));
        if let Some(premise) = premise
            && let Some((pattern, guards)) = propositions(premise).split_first()
            && rewrite::native_operation(self, pattern).is_some()
        {
            let policy = policy.ok_or_else(|| {
                Error("an operation rewrite requires a policy from std::rewrite::collect".into())
            })?;
            let (premises, actions) = rewrite::lower(self, policy, pattern, guards, head)?;
            return premises.into_iter().try_for_each(|facts| {
                rules
                    .rule(self.schema.database(), name, facts, actions.clone())
                    .map_err(error)
            });
        }
        let mut facts = Vec::new();
        if let Some(premise) = premise {
            let expanded = propositions(premise)
                .into_iter()
                .map(|expr| self.predicate_row(expr))
                .collect::<Result<Vec<_>, _>>()?;
            let clauses = expanded
                .iter()
                .map(|expr| expr.as_ref())
                .collect::<Vec<_>>();
            self.validate_inputs(&clauses)?;
            for expr in clauses {
                self.fact(expr, &mut facts)?;
            }
        }
        let actions = propositions(head)
            .iter()
            .map(|expr| self.action(expr))
            .collect::<Result<Vec<_>, _>>()?;
        rules
            .rule(
                self.schema.database(),
                name,
                ast::Facts(facts),
                ast::GenericActions(actions),
            )
            .map_err(error)
    }

    fn term(&self, expr: &Expr) -> Result<ast::Expr, Error> {
        let span = tir_egglog::span!();
        Ok(match &expr.value {
            Expression::Path(path) if self.schema.constructors.contains(path) => {
                ast::Expr::Call(span, self.name(path), vec![])
            }
            Expression::Path(path)
                if !self
                    .binders
                    .iter()
                    .any(|binder| path == &[binder.name.as_str()])
                    && self.captures.contains_key(path) =>
            {
                self.captures[path].clone()
            }
            Expression::Path(path) => ast::Expr::Var(span, path_name(path)),
            Expression::Member { value, name }
                if matches!(name.as_str(), "kind" | "typed_kind") =>
            {
                let name = self.operation(value, name).ok_or_else(|| {
                    Error("ordinary member projections are unsupported in executable rules".into())
                })?;
                ast::Expr::Call(span, name.symbol.clone(), vec![])
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
                        "operation_kind"
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
                self.name(path(value)?),
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
                let name = self.name(relation);
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
                let kind = self
                    .operation(operation, "matches")
                    .ok_or_else(|| Error("matches requires a declared operation".into()))?;
                let kind = ast::Expr::Call(span, kind.symbol.clone(), vec![]);
                return self.match_facts(kind, MatchPorts::parse(arguments)?, checked()?, facts);
            }
            if ir_function(value) == Some("match_kind") {
                let Some((kind, ports)) = arguments.split_first() else {
                    return Err(Error("match_kind requires an operation kind".into()));
                };
                let kind = self.term(kind)?;
                let ports = MatchPorts::parse(ports)?;
                facts.push(ast::Fact::Fact(ast::Expr::Call(
                    span,
                    "native-ports".into(),
                    vec![
                        kind.clone(),
                        tir_egglog::prelude::exprs::int(ports.operands.len() as i64),
                        tir_egglog::prelude::exprs::int(ports.results.len() as i64),
                    ],
                )));
                return self.match_facts(kind, ports, checked()?, facts);
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
            && self.schema.functions.contains(self.table_path(function))
        {
            let (last, keys) = arguments
                .split_last()
                .ok_or_else(|| Error("function row requires output".into()))?;
            ast::Fact::Eq(
                span,
                ast::Expr::Call(
                    span,
                    self.name(function),
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
            && self.schema.functions.contains(self.table_path(function))
        {
            let (last, keys) = arguments
                .split_last()
                .ok_or_else(|| Error("function row requires output".into()))?;
            return Ok(ast::Action::Set(
                span,
                self.name(function),
                keys.iter()
                    .map(|arg| self.term(arg))
                    .collect::<Result<_, _>>()?,
                self.term(last)?,
            ));
        }
        Ok(ast::Action::Expr(span, self.term(expr)?))
    }
}

struct FrameCode {
    bytes: Vec<u8>,
    locals: BTreeSet<String>,
    reads: Vec<String>,
}

impl ScopedFunction {
    fn compile(
        &self,
        db: &Database,
        rules: &Rules,
        templates: &BTreeSet<String>,
    ) -> Result<FrameCode, Error> {
        let rules = rules.select(std::slice::from_ref(&self.schedule), &[]);
        let mut stages = Vec::new();
        let mut pending = vec![&self.schedule];
        while let Some(schedule) = pending.pop() {
            match schedule {
                Schedule::Sequence(_, children) => pending.extend(children.iter().rev()),
                Schedule::Repeat(_, _, child) | Schedule::Saturate(_, child) => pending.push(child),
                Schedule::Run(..) => stages.push(schedule.clone()),
            }
        }
        let footprints = if self.memo {
            rules
                .materialization_footprints(db, stages.iter())
                .map_err(error)?
        } else {
            Vec::new()
        };
        let table_name = |table| db.schema().table(table).name();
        let mut locals = self.locals.iter().cloned().collect::<BTreeSet<_>>();
        for footprint in &footprints {
            for &table in &footprint.writes {
                let name = table_name(table);
                if templates.contains(name) {
                    locals.insert(name.to_owned());
                }
                if self.memo && !locals.contains(name) {
                    return Err(Error(format!(
                        "memo function writes outside its local tables: {name}"
                    )));
                }
            }
        }
        let mut reads = BTreeSet::new();
        let mut completed = BTreeSet::<tir_egglog::TableId>::new();
        for footprint in footprints {
            completed.extend(footprint.writes.iter().copied());
            for table in footprint.reads {
                let name = table_name(table);
                if locals.contains(name) {
                    if !completed.contains(&table) && !name.starts_with("@local::") {
                        return Err(Error(format!(
                            "schedule reads local table {name} before its producer"
                        )));
                    }
                } else {
                    reads.insert(name.to_owned());
                }
            }
        }
        let tables = locals
            .iter()
            .map(|name| db.table(name).map_err(error))
            .collect::<Result<_, _>>()?;
        let bytes = rules
            .compile_with_tables(db, &tables)
            .map_err(error)?
            .to_bytes();
        Ok(FrameCode {
            bytes,
            locals,
            reads: reads.into_iter().collect(),
        })
    }
}

struct ScopedQuery {
    parameters: Vec<crate::ast::Binder>,
    body: Expr,
    tables: BTreeMap<Path, Path>,
}

struct ScopedFunction {
    parameters: Vec<crate::ast::Binder>,
    schedule: Schedule,
    locals: Vec<String>,
    fields: Vec<(String, Path)>,
    query: Option<ScopedQuery>,
    value: Option<Expr>,
    memo: bool,
}

/// Typed rules ready for engine planning against the installed schema.
pub struct LoweredProgram {
    rules: Rules,
    schedules: BTreeMap<Path, Schedule>,
    scoped: BTreeMap<Path, ScopedFunction>,
    collections: BTreeMap<Path, String>,
}
impl LoweredProgram {
    /// Checked source schedules, with references bound to this program's rulesets.
    pub fn schedules(&self) -> &BTreeMap<Path, Schedule> {
        &self.schedules
    }
    /// Plan executable instructions against a database without replacing it.
    pub fn compile(&self, database: &Database) -> Result<tir_egglog::Program, Error> {
        if self.scoped.is_empty() {
            return self.rules.compile(database).map_err(error);
        }
        let schedules = self.schedules.values().cloned().collect::<Vec<_>>();
        let collections = self
            .collections
            .values()
            .map(String::as_str)
            .collect::<Vec<_>>();
        self.rules
            .select(&schedules, &collections)
            .compile(database)
            .map_err(error)
    }
}
