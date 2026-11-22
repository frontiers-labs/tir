use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{self, Expr, Item};
use crate::expander::Diag;
use crate::module_tree::{ModulePath, ModuleTree};
use crate::{Span, Type};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Kind {
    Isa,
    Abi,
    RegisterClass,
    Instruction,
    Template,
    ScheduleClass,
    Machine,
    Function,
}

impl Kind {
    fn description(self) -> &'static str {
        match self {
            Self::Isa => "ISA",
            Self::Abi => "ABI",
            Self::RegisterClass => "register class",
            Self::Instruction => "instruction",
            Self::Template => "template",
            Self::ScheduleClass => "scheduling class",
            Self::Machine => "machine",
            Self::Function => "function",
        }
    }

    fn emitted_as(self, name: &str) -> Option<String> {
        match self {
            Self::Instruction => Some(name.to_ascii_uppercase()),
            Self::RegisterClass | Self::Isa => Some(name.to_ascii_lowercase()),
            Self::Machine => Some(crate::rustgen::machine_symbol(name)),
            Self::Abi => Some(name.to_string()),
            Self::ScheduleClass | Self::Template | Self::Function => None,
        }
    }
}

fn item_identity(item: &mut Item) -> (Kind, &mut String, Span) {
    match item {
        Item::Isa(item) => (Kind::Isa, &mut item.name, item.span),
        Item::Abi(item) => (Kind::Abi, &mut item.name, item.span),
        Item::RegisterClass(item) => (Kind::RegisterClass, &mut item.name, item.span),
        Item::Instruction(item) => (Kind::Instruction, &mut item.name, item.span),
        Item::Template(item) => (Kind::Template, &mut item.name, item.span),
        Item::Unit(item) => (Kind::ScheduleClass, &mut item.name, item.span),
        Item::Machine(item) => (Kind::Machine, &mut item.name, item.span),
        Item::Fn(item) => (Kind::Function, &mut item.name, item.span),
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SymbolId {
    pub module: ModulePath,
    pub name: String,
}

impl SymbolId {
    pub fn new(module: &ModulePath, name: &str) -> Self {
        Self {
            module: module.clone(),
            name: name.into(),
        }
    }

    pub fn text(&self) -> String {
        self.module.qualify(&self.name)
    }

    pub fn resolve(module: &ModulePath, reference: &str) -> Option<Self> {
        let (module, name) = module.resolve(reference)?;
        Some(Self { module, name })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Names {
    #[cfg(all(feature = "json", feature = "rust-cli"))]
    Canonical,
    Documentation,
    Backend,
}

struct Symbol {
    kind: Kind,
    canonical: String,
}

#[derive(Default)]
struct Scope {
    bindings: BTreeMap<String, SymbolId>,
    macro_bindings: BTreeSet<String>,
}

impl Scope {
    fn import(
        &mut self,
        module: &ModulePath,
        reference: &str,
        symbols: &BTreeMap<SymbolId, Symbol>,
        macros: &BTreeSet<SymbolId>,
    ) -> Result<(), String> {
        let unresolved = || format!("unresolved import `{reference}`");
        let id = SymbolId::resolve(module, reference).ok_or_else(unresolved)?;
        let defines_macro = macros.contains(&id);
        let defines_item = symbols.contains_key(&id);
        if !defines_macro && !defines_item {
            return Err(unresolved());
        }
        let duplicate_macro = defines_macro && !self.macro_bindings.insert(id.name.clone());
        let duplicate_item =
            defines_item && self.bindings.insert(id.name.clone(), id.clone()).is_some();
        if duplicate_macro || duplicate_item {
            return Err(format!(
                "duplicate binding `{}` in module `{module}`",
                id.name
            ));
        }
        Ok(())
    }
}

fn diagnostic(file: &str, span: Span, message: String) -> Diag {
    (file.into(), chumsky::error::Rich::custom(span, message))
}

pub(crate) fn resolve(
    tree: &ModuleTree,
    files: &mut [ast::File],
    macro_names: impl IntoIterator<Item = SymbolId>,
    names: Names,
) -> Result<OutputNames, Vec<Diag>> {
    let mut diagnostics = Vec::new();
    let mut scopes = BTreeMap::<ModulePath, Scope>::new();
    let mut symbols = BTreeMap::new();
    let mut emitted = BTreeMap::<(Kind, String), SymbolId>::new();
    let mut leaves = BTreeMap::<String, usize>::new();

    let macro_names: BTreeSet<_> = macro_names.into_iter().collect();
    for id in &macro_names {
        scopes
            .entry(id.module.clone())
            .or_default()
            .macro_bindings
            .insert(id.name.clone());
    }
    for (module, file) in tree.modules().zip(files.iter_mut()) {
        let scope = scopes.entry(module.path.clone()).or_default();
        for item in &mut file.items {
            let (kind, name, span) = item_identity(item);
            let id = SymbolId::new(&module.path, name);
            if symbols.contains_key(&id) {
                diagnostics.push(diagnostic(
                    &file.file_name,
                    span,
                    format!("duplicate declaration `{}`", id.text(),),
                ));
                continue;
            }
            if matches!(names, Names::Backend)
                && matches!(
                    (kind, name.as_str()),
                    (Kind::RegisterClass, "Ordering") | (Kind::Isa, "Custom")
                )
            {
                diagnostics.push(diagnostic(
                    &file.file_name,
                    span,
                    format!(
                        "{} name `{name}` is reserved by the backend",
                        kind.description()
                    ),
                ));
            }
            if matches!(names, Names::Backend)
                && let Some(emitted_name) = kind.emitted_as(name)
                && let Some(first) = emitted.insert((kind, emitted_name), id.clone())
            {
                diagnostics.push(diagnostic(
                    &file.file_name,
                    span,
                    format!(
                        "emitted {} name collision between `{}` and `{}`",
                        kind.description(),
                        first.text(),
                        id.text(),
                    ),
                ));
            }
            *leaves.entry(name.clone()).or_default() += 1;
            scope.bindings.insert(name.clone(), id.clone());
            let canonical = id.text();
            symbols.insert(id, Symbol { kind, canonical });
        }
    }

    let output_names = OutputNames::for_symbols(&symbols, &leaves, names);

    for module in tree.modules() {
        let file = module.source_path.to_string_lossy();
        let scope = scopes
            .get_mut(&module.path)
            .expect("scope for every module");
        for (reference, span) in &module.imports {
            if let Err(message) = scope.import(&module.path, reference, &symbols, &macro_names) {
                diagnostics.push(diagnostic(&file, *span, message));
            }
        }
    }

    if matches!(names, Names::Backend) {
        for (module, file) in tree.modules().zip(files.iter_mut()) {
            for item in &mut file.items {
                let (kind, name, span) = item_identity(item);
                let id = SymbolId::new(&module.path, name);
                if kind.emitted_as(name).is_some()
                    && symbols
                        .get(&id)
                        .is_some_and(|symbol| !output_names.0.contains_key(&symbol.canonical))
                {
                    diagnostics.push(diagnostic(
                        &file.file_name,
                        span,
                        format!("emitted declaration name collision for `{}`", id.text(),),
                    ));
                }
            }
        }
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    for (module, file) in tree.modules().zip(files) {
        let scope = &scopes[&module.path];
        let mut lookup = |name: &str, expected: Kind, span: Span| {
            let resolved = if name.contains("::") {
                SymbolId::resolve(&module.path, name)
                    .as_ref()
                    .and_then(|id| symbols.get(id))
            } else {
                scope.bindings.get(name).and_then(|id| symbols.get(id))
            };
            match resolved {
                Some(symbol) if symbol.kind == expected => Some(symbol.canonical.clone()),
                Some(symbol) => {
                    diagnostics.push(diagnostic(
                        &file.file_name,
                        span,
                        format!(
                            "expected {} for `{name}`, found {}",
                            expected.description(),
                            symbol.kind.description(),
                        ),
                    ));
                    None
                }
                None => {
                    diagnostics.push(diagnostic(
                        &file.file_name,
                        span,
                        format!(
                            "unknown {} `{name}` in module `{}`",
                            expected.description(),
                            module.path,
                        ),
                    ));
                    None
                }
            }
        };
        let mut resolver = Resolver {
            lookup: &mut lookup,
        };
        for item in &mut file.items {
            let (_, name, _) = item_identity(item);
            let id = SymbolId::new(&module.path, name);
            *name = symbols[&id].canonical.clone();
            resolver.item(item);
        }
    }
    if diagnostics.is_empty() {
        Ok(output_names)
    } else {
        Err(diagnostics)
    }
}

#[derive(Default)]
pub(crate) struct OutputNames(BTreeMap<String, String>);

impl OutputNames {
    fn for_symbols(
        symbols: &BTreeMap<SymbolId, Symbol>,
        leaves: &BTreeMap<String, usize>,
        names: Names,
    ) -> Self {
        let mut output_names = Self::default();
        let mut backend_leaves = BTreeMap::<&str, usize>::new();
        for (id, symbol) in symbols {
            if symbol.kind.emitted_as(&id.name).is_some() {
                *backend_leaves.entry(&id.name).or_default() += 1;
            }
        }
        for (id, symbol) in symbols {
            let unique_leaf = leaves.get(&id.name) == Some(&1);
            let emitted = symbol.kind.emitted_as(&id.name).is_some();
            let keep_leaf = match names {
                #[cfg(all(feature = "json", feature = "rust-cli"))]
                Names::Canonical => false,
                Names::Documentation => unique_leaf,
                Names::Backend => {
                    unique_leaf || (emitted && backend_leaves.get(id.name.as_str()) == Some(&1))
                }
            };
            if keep_leaf {
                output_names
                    .0
                    .insert(symbol.canonical.clone(), id.name.clone());
            }
        }
        output_names
    }

    pub fn lower(&self, files: &mut [ast::File]) {
        if self.0.is_empty() {
            return;
        }
        let mut lookup = |name: &str, _: Kind, _: Span| self.0.get(name).cloned();
        let mut resolver = Resolver {
            lookup: &mut lookup,
        };
        for item in files.iter_mut().flat_map(|file| &mut file.items) {
            let (_, name, _) = item_identity(item);
            if let Some(lowered) = self.0.get(name) {
                name.clone_from(lowered);
            }
            resolver.item(item);
        }
    }
}

struct Resolver<'a> {
    lookup: &'a mut dyn FnMut(&str, Kind, Span) -> Option<String>,
}

impl Resolver<'_> {
    fn name(&mut self, name: &mut String, kind: Kind, span: Span) {
        if let Some(resolved) = (self.lookup)(name, kind, span) {
            *name = resolved;
        }
    }

    fn names(&mut self, names: &mut [String], kind: Kind, span: Span) {
        for name in names {
            self.name(name, kind, span);
        }
    }

    fn optional(&mut self, name: &mut Option<String>, kind: Kind, span: Span) {
        if let Some(name) = name {
            self.name(name, kind, span);
        }
    }

    fn item(&mut self, item: &mut Item) {
        match item {
            Item::Isa(isa) => {
                match &mut isa.requires {
                    Some(ast::IsaRequirement::Single(name)) => self.name(name, Kind::Isa, isa.span),
                    Some(ast::IsaRequirement::Any(names) | ast::IsaRequirement::All(names)) => {
                        self.names(names, Kind::Isa, isa.span);
                    }
                    None => {}
                }
                self.params(&mut isa.parameters, isa.span);
                if let Some(trap) = &mut isa.trap_handler {
                    trap.body = self.expr(&trap.body, false);
                }
            }
            Item::Abi(abi) => self.abi(abi),
            Item::RegisterClass(class) => {
                self.names(&mut class.for_isas, Kind::Isa, class.span);
                self.optional(&mut class.base, Kind::RegisterClass, class.span);
                self.optional(&mut class.file, Kind::RegisterClass, class.span);
                self.params(&mut class.parameters, class.span);
            }
            Item::Template(template) => {
                self.names(&mut template.for_isas, Kind::Isa, template.span);
                self.optional(&mut template.parent_template, Kind::Template, template.span);
                self.params(&mut template.params, template.span);
                self.operands(&mut template.operands, template.span);
                self.optional_expr(&mut template.encoding);
                self.optional_expr(&mut template.asm);
                self.schedule(&mut template.schedule);
            }
            Item::Instruction(inst) => {
                self.names(&mut inst.for_isas, Kind::Isa, inst.span);
                self.optional(&mut inst.parent_template, Kind::Template, inst.span);
                self.params(&mut inst.params, inst.span);
                self.operands(&mut inst.operands, inst.span);
                self.optional_expr(&mut inst.encoding);
                self.optional_expr(&mut inst.asm);
                self.schedule(&mut inst.schedule);
                inst.behavior = self.expr(&inst.behavior, false);
            }
            Item::Machine(machine) => self.machine(machine),
            Item::Fn(function) => {
                for param in &mut function.params {
                    if let Some(ty) = &mut param.ty {
                        self.ty(ty, function.span);
                    }
                }
                function.body = self.expr(&function.body, false);
            }
            Item::Unit(_) => {}
        }
    }

    fn abi(&mut self, abi: &mut ast::Abi) {
        self.names(&mut abi.for_isas, Kind::Isa, abi.span);
        self.optional(&mut abi.base, Kind::Abi, abi.span);
        self.params(&mut abi.parameters, abi.span);
        if let Some(stack) = &mut abi.stack {
            self.optional_expr(&mut stack.align);
            self.optional_expr(&mut stack.red_zone);
            self.optional_expr(&mut stack.slot_size);
        }
        for role in &mut abi.roles {
            self.name(&mut role.register.class, Kind::RegisterClass, role.span);
        }
        let sequences = abi
            .callee_saved
            .iter_mut()
            .flatten()
            .chain(abi.reserved.iter_mut().flatten())
            .chain(
                abi.args
                    .iter_mut()
                    .chain(&mut abi.rets)
                    .flat_map(|pass| &mut pass.registers),
            );
        for sequence in sequences {
            for register in std::iter::once(&mut sequence.start).chain(&mut sequence.end) {
                self.name(&mut register.class, Kind::RegisterClass, register.span);
            }
        }
    }

    fn machine(&mut self, machine: &mut ast::Machine) {
        self.names(&mut machine.for_isas, Kind::Isa, machine.span);
        for (name, _) in &mut machine.reg_files {
            self.name(name, Kind::RegisterClass, machine.span);
        }
        for bind in &mut machine.binds {
            self.name(&mut bind.unit, Kind::ScheduleClass, bind.span);
        }
        for entry in &mut machine.overrides {
            self.name(&mut entry.instruction, Kind::Instruction, entry.span);
            for case in &mut entry.latency_cases {
                case.condition = self.expr(&case.condition, false);
            }
        }
        for fusion in &mut machine.fusions {
            for step in &mut fusion.steps {
                self.names(&mut step.instruction_names, Kind::Instruction, step.span);
            }
            if let Some(condition) = &mut fusion.condition {
                *condition = self.expr(condition, true);
            }
        }
    }

    fn schedule(&mut self, schedule: &mut Option<ast::Schedule>) {
        if let Some(schedule) = schedule {
            self.names(&mut schedule.classes, Kind::ScheduleClass, schedule.span);
        }
    }

    fn params(
        &mut self,
        params: &mut crate::utils::StableHashMap<String, (Type, Option<Expr>)>,
        span: Span,
    ) {
        *params = std::mem::take(params)
            .into_iter()
            .map(|(name, (mut ty, mut expr))| {
                self.ty(&mut ty, span);
                self.optional_expr(&mut expr);
                (name, (ty, expr))
            })
            .collect();
    }

    fn operands(&mut self, operands: &mut [ast::Operand], span: Span) {
        for operand in operands {
            self.ty(&mut operand.ty, span);
        }
    }

    fn ty(&mut self, ty: &mut Type, span: Span) {
        match ty {
            Type::Struct(name) => self.name(name, Kind::RegisterClass, span),
            Type::BitsExpr(expr) => **expr = self.expr(expr, false),
            _ => {}
        }
    }

    fn optional_expr(&mut self, expr: &mut Option<Expr>) {
        if let Some(expr) = expr {
            *expr = self.expr(expr, false);
        }
    }

    fn expr(&mut self, expr: &Expr, fusion_guard: bool) -> Expr {
        match expr {
            Expr::Path(path) if path.base != "Ordering" || path.remainder.len() != 1 => {
                let Some((member, modules)) = path.remainder.split_last() else {
                    return expr.clone();
                };
                let name = if modules.is_empty() {
                    path.base.clone()
                } else {
                    format!("{}::{}", path.base, modules.join("::"))
                };
                match (self.lookup)(&name, Kind::RegisterClass, path.span) {
                    Some(base) => Expr::Path(ast::Path {
                        base,
                        remainder: vec![member.clone()],
                        span: path.span,
                    }),
                    None => expr.clone(),
                }
            }
            Expr::Call(call) => {
                let mut call = call.clone();
                for arg in &mut call.arguments {
                    *arg = self.expr(arg, fusion_guard);
                }
                let function = match call.callee.as_ref() {
                    Expr::Ident(id) => Some((id.name.clone(), id.span)),
                    Expr::Path(path) => Some((
                        format!("{}::{}", path.base, path.remainder.join("::")),
                        path.span,
                    )),
                    _ => None,
                };
                if let Some((name, span)) = function
                    && !(fusion_guard && crate::fninline::is_fusion_builtin(&name))
                    && let Some(resolved) = (self.lookup)(&name, Kind::Function, span)
                {
                    *call.callee = ast::Ident::new(resolved, span).into();
                }
                Expr::Call(call)
            }
            Expr::Assign(assignment) => {
                let mut assignment = assignment.clone();
                *assignment.dest = self.expr(&assignment.dest, fusion_guard);
                *assignment.value = self.expr(&assignment.value, fusion_guard);
                Expr::Assign(assignment)
            }
            other => {
                crate::utils::map_child_exprs(other, &mut |child| self.expr(child, fusion_guard))
            }
        }
    }
}
