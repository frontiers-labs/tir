//! Executable lowering against an explicitly shared engine schema.
mod operation;

use std::collections::{BTreeMap, BTreeSet};

use tir_egglog::{Database, EGraph, Merge, Rules, Schedule, SortId, SortKind, TableKind, ast};

use crate::{
    CheckedProgram, Error,
    ast::{BinaryOp, Declaration, Expr, Expression, Literal, Path, UnaryOp},
};

/// Reusable schema and host bindings, shared by independently compiled programs.
pub struct Schema {
    engine: EGraph,
    names: BTreeMap<Path, String>,
    functions: BTreeSet<Path>,
    constructors: BTreeSet<Path>,
}

fn error(error: impl std::fmt::Display) -> Error {
    Error(error.to_string())
}
fn path_name(path: &[String]) -> String {
    path.join("::")
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
fn proposition(expr: &Expr) -> bool {
    matches!(&expr.value, Expression::Path(path) if path == &["Prop"])
}
fn clause(expr: &Expr) -> bool {
    match &expr.value {
        Expression::Call { .. }
        | Expression::Unary {
            op: UnaryOp::Not, ..
        }
        | Expression::Binary {
            op: BinaryOp::Equal,
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
    /// Start with the tables from a compiled catalog.
    pub fn from_catalog(bytes: &[u8]) -> Result<Self, Error> {
        let mut engine = EGraph::default();
        tir_egglog::Program::from_bytes(bytes, engine.database_mut()).map_err(error)?;
        Ok(Self {
            engine,
            names: BTreeMap::new(),
            functions: BTreeSet::new(),
            constructors: BTreeSet::new(),
        })
    }

    /// Install declarations once, using canonical CDSL paths as engine names.
    pub fn new(program: &CheckedProgram) -> Result<Self, Error> {
        Self::with_names(program, BTreeMap::new())
    }

    /// Install declarations with explicit host ABI names for selected paths.
    pub fn with_names(
        program: &CheckedProgram,
        names: BTreeMap<Path, String>,
    ) -> Result<Self, Error> {
        let mut schema = Self {
            engine: EGraph::default(),
            names,
            functions: BTreeSet::new(),
            constructors: BTreeSet::new(),
        };
        schema.extend(program)?;
        Ok(schema)
    }

    /// Bind bare checked names to their installed engine names.
    pub fn bind_names<'a>(&mut self, names: impl IntoIterator<Item = (&'a str, &'a str)>) {
        self.names.extend(
            names
                .into_iter()
                .map(|(source, host)| (vec![source.into()], host.into())),
        );
    }

    /// The database whose declarations type and bind every compiled program.
    pub fn database(&self) -> &Database {
        self.engine.database()
    }

    /// Install additional checked declarations without importing their rules.
    pub fn extend(&mut self, program: &CheckedProgram) -> Result<(), Error> {
        let modules = program
            .artifact()
            .all_modules()
            .filter(|module| module.package != "<embedded std>")
            .collect::<Vec<_>>();
        for module in &modules {
            for declaration in &module.ast.declarations {
                if let Declaration::Axiom {
                    name, generics, ty, ..
                } = &declaration.value
                    && matches!(&ty.value, Expression::Path(path) if path == &["Type"])
                {
                    if !generics.is_empty() {
                        return Err(Error(
                            "generic executable sorts require specialization".into(),
                        ));
                    }
                    let mut full = module.path.clone();
                    full.push(name.clone());
                    let name = self.name(&full);
                    if self.database().schema().sort_named(&name).is_none() {
                        self.engine.database_mut().add_sort(&name, SortKind::Eq);
                    }
                }
                if let Declaration::Enum { name, generics, .. } = &declaration.value {
                    if !generics.is_empty() {
                        return Err(Error(
                            "generic executable enums require specialization".into(),
                        ));
                    }
                    let mut full = module.path.clone();
                    full.push(name.clone());
                    let name = self.name(&full);
                    if self.database().schema().sort_named(&name).is_none() {
                        self.engine.database_mut().add_sort(&name, SortKind::Eq);
                    }
                }
            }
        }
        for module in &modules {
            for declaration in &module.ast.declarations {
                if let Declaration::Enum { name, variants, .. } = &declaration.value {
                    let mut full = module.path.clone();
                    full.push(name.clone());
                    let out = self.sort_path(&full)?;
                    if self.database().schema().sort(out).kind() != SortKind::Eq {
                        return Err(Error(format!(
                            "enum {} requires an equivalence sort",
                            self.name(&full)
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
                        let name = self.name(&variant_path);
                        if let Some(id) = self.database().schema().table_named(&name) {
                            let table = self.database().schema().table(id);
                            let mut columns = args.clone();
                            columns.push(out);
                            if table.kind() != TableKind::Constructor || table.columns() != columns
                            {
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
        }
        // Table descriptors declare keyed storage. Proposition declarations can
        // expose those same rows without changing their functional dependency.
        for module in &modules {
            for declaration in &module.ast.declarations {
                if let Declaration::Axiom {
                    name: declared, ty, ..
                } = &declaration.value
                    && let Expression::Generic { value, arguments } = &ty.value
                    && path(value).is_ok_and(|p| p == &["std", "table", "Table"])
                {
                    let [key, out, merge] = arguments.as_slice() else {
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
                            if matches!(
                                self.database().schema().sort(out).kind(),
                                SortKind::Base(_)
                            ) {
                                return Err(Error(
                                    "Union merge requires values with equivalence identities"
                                        .into(),
                                ));
                            }
                            Merge::Union
                        }
                        _ => return Err(Error("unsupported executable table merge".into())),
                    };
                    let mut full = module.path.clone();
                    full.push(declared.clone());
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
                }
            }
        }
        for module in &modules {
            for declaration in &module.ast.declarations {
                let Declaration::Axiom {
                    name: declared, ty, ..
                } = &declaration.value
                else {
                    continue;
                };
                let Some((args, result)) = arrow(ty) else {
                    continue;
                };
                if !proposition(result) {
                    continue;
                }
                let mut full = module.path.clone();
                full.push(declared.clone());
                let name = self.name(&full);
                let columns = parts(args)
                    .iter()
                    .map(|ty| self.sort(ty))
                    .collect::<Result<Vec<_>, _>>()?;
                if let Some(id) = self.database().schema().table_named(&name) {
                    if self.database().schema().table(id).columns() != columns
                        || self.database().schema().table(id).kind() == TableKind::Constructor
                    {
                        return Err(Error(format!("incompatible declaration {name}")));
                    }
                    if self.database().schema().table(id).kind() == TableKind::Function {
                        self.functions.insert(full);
                    }
                } else {
                    self.engine.database_mut().add_relation(&name, &columns);
                }
            }
        }
        Ok(())
    }

    /// Serialize declarations independently of executable programs.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(Rules::compile_schema(self.database())
            .map_err(error)?
            .to_bytes())
    }

    /// Compile a checked program against this schema and serialize its rules.
    pub fn compile_bytes(&self, program: &CheckedProgram) -> Result<Vec<u8>, Error> {
        Ok(self.lower(program)?.compile(self.database())?.to_bytes())
    }

    fn name(&self, path: &[String]) -> String {
        self.names.get(path).cloned().unwrap_or_else(|| match path {
            [namespace, module, name] if namespace == "std" && module == "bigint" => {
                match name.as_str() {
                    "BigInt" => "BigInt".into(),
                    "add" => "+".into(),
                    "sub" => "-".into(),
                    "from_string" => "from-string".into(),
                    "ge" => "bool->=".into(),
                    _ => path_name(path),
                }
            }
            _ => path_name(path),
        })
    }
    fn sort_path(&self, path: &[String]) -> Result<SortId, Error> {
        let name = match path {
            [name] if name == "Int" => "i64".into(),
            [name] if name == "Bool" => "bool".into(),
            [name] if name == "String" => "String".into(),
            _ => self.name(path),
        };
        self.database().sort(&name).map_err(error)
    }
    fn sort(&self, expr: &Expr) -> Result<SortId, Error> {
        self.sort_path(path(expr)?)
    }

    /// Lower checked source into typed engine rules without planning or loading.
    pub fn lower(&self, program: &CheckedProgram) -> Result<LoweredProgram, Error> {
        let mut rules = Rules::new();
        let mut modules = BTreeMap::new();
        let mut definitions = BTreeMap::new();
        for module in &program.artifact().modules {
            let ruleset = self.name(&module.path);
            for declaration in &module.ast.declarations {
                if let Declaration::Let { binding, .. } = &declaration.value {
                    let mut path = module.path.clone();
                    path.extend(binding.path.clone());
                    if !binding.generics.is_empty() {
                        return Err(Error(format!(
                            "unsupported executable generic definition {}",
                            self.name(&path)
                        )));
                    }
                    definitions.insert(path, binding.value.clone());
                    continue;
                }
                let Declaration::Axiom { name, ty, .. } = &declaration.value else {
                    continue;
                };
                if !clause(ty) {
                    let schema = matches!(&ty.value, Expression::Path(path) if path == &["Type"])
                        || arrow(ty).is_some_and(|(_, result)| proposition(result))
                        || matches!(&ty.value, Expression::Generic { value, .. } if path(value).is_ok_and(|path| path == &["std", "table", "Table"]));
                    let mut path = module.path.clone();
                    path.push(name.clone());
                    let primitive = arrow(ty).is_some_and(|(args, output)| {
                        let Ok(inputs) = parts(args)
                            .iter()
                            .map(|arg| self.sort(arg))
                            .collect::<Result<Vec<_>, _>>()
                        else {
                            return false;
                        };
                        let Ok(output) = self.sort(output) else {
                            return false;
                        };
                        let schema = self.database().schema();
                        schema.primitives(&self.name(&path)).any(|id| {
                            let (_, signature) = schema.primitive(id);
                            signature.inputs == inputs && signature.output == output
                        })
                    });
                    if !schema && !primitive {
                        return Err(Error(format!(
                            "unsupported executable axiom declaration: {name}"
                        )));
                    }
                    continue;
                }
                rules.add_ruleset(&ruleset);
                modules.insert(module.path.clone(), ruleset.clone());
                let (premise, head) = arrow(ty).map_or((None, ty), |(a, b)| (Some(a), b));
                let facts = premise
                    .map(|expr| {
                        propositions(expr)
                            .iter()
                            .map(|expr| self.fact(expr))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let actions = propositions(head)
                    .iter()
                    .map(|expr| self.action(expr))
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
        }
        let mut schedules = BTreeMap::new();
        for (path, expression) in &definitions {
            let schedule = self.schedule(
                expression,
                &self.name(path),
                &modules,
                &definitions,
                &mut BTreeSet::from([path.clone()]),
                &mut rules,
            )?;
            schedules.insert(path.clone(), schedule);
        }
        Ok(LoweredProgram { rules, schedules })
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
            | (["std", "schedule", "Schedule", "Saturate"], [bundle])
            | (["std", "schedule", "saturate"], [bundle]) => {
                let mut members = self.bundle(bundle, modules)?;
                members.sort();
                members.dedup();
                rules.add_combined_ruleset(name, &members).map_err(error)?;
                let run = Schedule::run(name);
                Ok(if callee.last() == Some(&"Rules") {
                    run
                } else {
                    Schedule::saturate(run)
                })
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

    fn term(&self, expr: &Expr) -> Result<ast::Expr, Error> {
        let span = tir_egglog::span!();
        Ok(match &expr.value {
            Expression::Path(path) if self.constructors.contains(path) => {
                ast::Expr::Call(span, self.name(path), vec![])
            }
            Expression::Path(path) => ast::Expr::Var(span, path_name(path)),
            Expression::Literal(literal) => ast::Expr::Lit(
                span,
                match literal {
                    Literal::Natural(n) => ast::Literal::Int(n.parse().map_err(error)?),
                    Literal::Bool(b) => ast::Literal::Bool(*b),
                    Literal::String(s) => ast::Literal::String(s.clone()),
                },
            ),
            Expression::Call { value, arguments } => ast::Expr::Call(
                span,
                self.name(path(value)?),
                arguments
                    .iter()
                    .map(|arg| self.term(arg))
                    .collect::<Result<_, _>>()?,
            ),
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
    fn fact(&self, expr: &Expr) -> Result<ast::Fact, Error> {
        let span = tir_egglog::span!();
        if let Expression::Unary {
            op: UnaryOp::Not,
            value,
        } = &expr.value
        {
            return Ok(ast::Fact::Not(self.term(value)?));
        }
        if let Expression::Binary {
            op: BinaryOp::Equal,
            left,
            right,
        } = &expr.value
        {
            return Ok(ast::Fact::Eq(span, self.term(left)?, self.term(right)?));
        }
        if let Expression::Call { value, arguments } = &expr.value
            && self.functions.contains(path(value)?)
        {
            let (last, keys) = arguments
                .split_last()
                .ok_or_else(|| Error("function row requires output".into()))?;
            return Ok(ast::Fact::Eq(
                span,
                ast::Expr::Call(
                    span,
                    self.name(path(value)?),
                    keys.iter()
                        .map(|arg| self.term(arg))
                        .collect::<Result<_, _>>()?,
                ),
                self.term(last)?,
            ));
        }
        Ok(ast::Fact::Fact(self.term(expr)?))
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
            && self.functions.contains(path(value)?)
        {
            let (last, keys) = arguments
                .split_last()
                .ok_or_else(|| Error("function row requires output".into()))?;
            return Ok(ast::Action::Set(
                span,
                self.name(path(value)?),
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
