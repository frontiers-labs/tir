#[path = "compiler_imports.rs"]
mod imports;
#[path = "compiler_sources.rs"]
mod sources;

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    ast::{Ast, Declaration, Node, Path as CdslPath},
    error::Error,
    lexer::{Span, TokenStream},
    parser,
};

#[derive(Clone, Debug)]
pub struct CompiledModule {
    pub package: String,
    pub root_depth: usize,
    pub path: Vec<String>,
    pub file: PathBuf,
    pub ast: Ast,
}

#[derive(Clone, Debug)]
pub struct Program {
    pub modules: Vec<CompiledModule>,
    support: Vec<CompiledModule>,
    pub(crate) module_exports: BTreeMap<Vec<String>, BTreeMap<String, Vec<String>>>,
    pub(crate) references: BTreeMap<CdslPath, Vec<BTreeSet<CdslPath>>>,
    declaration_sources: BTreeMap<CdslPath, String>,
}

impl Program {
    pub(crate) fn declaration_source(&self, module: &[String]) -> &str {
        self.declaration_sources
            .get(module)
            .map_or("anonymous", String::as_str)
    }

    pub(crate) fn all_modules(&self) -> impl Iterator<Item = &CompiledModule> {
        self.modules.iter().chain(&self.support)
    }
    /// Recheck this revision and retain an immutable checked source artifact.
    pub fn lower(&self) -> Result<crate::CheckedProgram, Error> {
        let selected = self
            .modules
            .iter()
            .map(|module| (module.package.clone(), module.path.clone()))
            .collect::<BTreeSet<_>>();
        let mut snapshot = self.clone();
        snapshot.modules.append(&mut snapshot.support);
        resolve_names(&mut snapshot, true)?;
        let (modules, support) = snapshot
            .modules
            .into_iter()
            .partition(|module| selected.contains(&(module.package.clone(), module.path.clone())));
        snapshot.modules = modules;
        snapshot.support = support;
        crate::infer::lower(&snapshot)
    }
    pub fn render(&self) -> String {
        let mut result = String::new();
        for module in &self.modules {
            result.push_str(&format!(
                "module ::{} file {}\n",
                module.path.join("::"),
                module.file.display()
            ));
            for declaration in &module.ast.declarations {
                result.push_str(&format!("{:#?}\n", declaration.value));
            }
        }
        result
    }

    pub fn select(mut self, selector: &[String]) -> Result<Self, Error> {
        let mut modules = Vec::new();
        for module in self.modules {
            if module.path.starts_with(selector) {
                modules.push(module);
            } else {
                self.support.push(module);
            }
        }
        if modules.is_empty() {
            return Err(Error(format!(
                "module ::{} does not exist",
                selector.join("::")
            )));
        }
        Ok(Self {
            modules,
            support: self.support,
            module_exports: self.module_exports,
            references: self.references,
            declaration_sources: self.declaration_sources,
        })
    }

    pub(crate) fn exported_module<'a>(&'a self, selector: &'a [String]) -> Option<&'a [String]> {
        let mut selected = selector;
        if !self.module_exports.contains_key(selected) {
            selected = &[];
            for component in selector {
                selected = self.module_exports.get(selected)?.get(component)?;
            }
        }
        self.module_exports
            .contains_key(selected)
            .then_some(selected)
    }
}

pub fn compile_file(file: &Path) -> Result<Program, Error> {
    finish(load_file(file)?)
}

pub(crate) fn check_file(file: &Path) -> Result<crate::CheckedProgram, Error> {
    let program = resolve_package(load_file(file)?)?;
    crate::infer::lower(&separate_embedded_std(program))
}

fn load_file(file: &Path) -> Result<Program, Error> {
    let root = file.parent().unwrap_or(Path::new("."));
    let mut program = Program {
        modules: Vec::new(),
        support: Vec::new(),
        module_exports: BTreeMap::new(),
        references: BTreeMap::new(),
        declaration_sources: BTreeMap::new(),
    };
    load_module(
        &mut program,
        "",
        root,
        file,
        Vec::new(),
        0,
        &mut BTreeSet::new(),
    )?;
    Ok(program)
}

/// Compile standalone source through the shared parser, resolver and checker.
pub fn compile_source(source: &str) -> Result<Program, Error> {
    let file = Path::new("stdin.cdsl");
    compile_single(parse_source(source, file)?, file)
}

/// Compile a workspace member's defs/lib.cdsl, located by its package name.
pub fn resolve_crate(name: &str) -> Result<Program, Error> {
    compile_file(&resolve_package_defs(name, false)?)
}

/// Resolve the package prefix of a build selector by Cargo name or directory.
pub fn resolve_program_package(prefix: &str) -> Result<Program, Error> {
    compile_file(&resolve_package_defs(prefix, true)?)
}

pub(crate) fn resolve_package_defs(name: &str, allow_directory: bool) -> Result<PathBuf, Error> {
    let current = std::env::current_dir().map_err(|error| Error(error.to_string()))?;
    for root in current.ancestors() {
        let manifest = root.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let source = fs::read_to_string(&manifest)
            .map_err(|error| Error(format!("{}: {error}", manifest.display())))?;
        let Some(members) = manifest_field(&source, "workspace", "members") else {
            continue;
        };
        let members = members
            .trim_start()
            .strip_prefix('[')
            .and_then(|members| members.split_once(']'))
            .ok_or_else(|| {
                Error(format!(
                    "{}: expected workspace members array",
                    manifest.display()
                ))
            })?
            .0;
        for member in members.split('"').skip(1).step_by(2) {
            let directory = root.join(member);
            let manifest = directory.join("Cargo.toml");
            let source = fs::read_to_string(&manifest)
                .map_err(|error| Error(format!("{}: {error}", manifest.display())))?;
            if manifest_field(&source, "package", "name")
                .and_then(|value| value.trim_start().strip_prefix('"'))
                .and_then(|value| value.split_once('"'))
                .is_some_and(|(package, _)| package == name)
                || (allow_directory && directory.file_name().is_some_and(|part| part == name))
            {
                return Ok(directory.join("defs/lib.cdsl"));
            }
        }
        return Err(Error(format!(
            "Cargo package {name} does not exist in {}",
            root.display()
        )));
    }
    Err(Error("no Cargo workspace found".into()))
}

fn manifest_field<'a>(source: &'a str, table: &str, key: &str) -> Option<&'a str> {
    let header = format!("[{table}]");
    let mut active = false;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            active = trimmed == header;
        } else if active
            && let Some((name, _)) = line.split_once('=')
            && name.trim() == key
        {
            return Some(&source[offset + line.find('=')? + 1..]);
        }
        offset += line.len();
    }
    None
}

/// Compile inline Rust tokens through the same parser, resolver and checker as files.
pub fn compile_tokens(input: proc_macro::TokenStream) -> Result<Program, Error> {
    let tokens = TokenStream::from_proc_macro(input).map_err(|errors| {
        Error(
            errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })?;
    let ast = parser::parse(tokens).map_err(|errors| {
        Error(
            errors
                .into_iter()
                .map(|e| {
                    format!(
                        "unexpected {:?}, expected {}",
                        e.found,
                        e.expected.join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })?;
    compile_single(ast, Path::new("inline.cdsl"))
}
fn compile_single(ast: Ast, file: &Path) -> Result<Program, Error> {
    if ast.declarations.iter().any(|d| {
        matches!(
            d.value,
            Declaration::Mod {
                declarations: None,
                ..
            }
        )
    }) {
        return Err(Error(
            "inline modules need an explicit caller-owned file root".into(),
        ));
    }
    let program = Program {
        modules: vec![CompiledModule {
            package: String::new(),
            root_depth: 0,
            path: Vec::new(),
            file: file.file_name().unwrap_or_default().into(),
            ast,
        }],
        support: Vec::new(),
        module_exports: BTreeMap::new(),
        references: BTreeMap::new(),
        declaration_sources: BTreeMap::new(),
    };
    finish(program)
}

fn expand_inline_modules(program: &mut Program) {
    let mut index = 0;
    while index < program.modules.len() {
        let module = &mut program.modules[index];
        let mut children = Vec::new();
        for declaration in &mut module.ast.declarations {
            if let Declaration::Mod {
                name,
                declarations: Some(declarations),
                ..
            } = &mut declaration.value
            {
                let mut path = module.path.clone();
                path.push(name.clone());
                if let Some(owner) = program.declaration_sources.get(&module.path).cloned() {
                    program.declaration_sources.insert(path.clone(), owner);
                }
                children.push(CompiledModule {
                    package: module.package.clone(),
                    root_depth: module.root_depth,
                    path,
                    file: module.file.clone(),
                    ast: Ast {
                        declarations: std::mem::take(declarations),
                        token_spans: module.ast.token_spans.clone(),
                        end: module.ast.end.clone(),
                    },
                });
            }
        }
        program.modules.extend(children);
        index += 1;
    }
}

fn finish(program: Program) -> Result<Program, Error> {
    let program = resolve_package(program)?;
    crate::infer::check(&program)?;
    Ok(separate_embedded_std(program))
}

fn resolve_package(mut program: Program) -> Result<Program, Error> {
    load_source(
        &mut program,
        "<embedded std>",
        sources::Source::embedded_std(),
        vec!["std".into()],
        0,
        &mut BTreeSet::new(),
    )?;
    expand_inline_modules(&mut program);
    resolve_names(&mut program, false)?;
    Ok(program)
}

fn separate_embedded_std(mut program: Program) -> Program {
    let (modules, support) = program
        .modules
        .into_iter()
        .partition(|m| m.package != "<embedded std>");
    program.modules = modules;
    program.support = support;
    program
}

fn parse_source(source: &str, file: &Path) -> Result<Ast, Error> {
    let stream = TokenStream::from_source(source).map_err(|errors| {
        Error(
            errors
                .into_iter()
                .map(|error| {
                    let offset = match error.span {
                        Span::Source(range) => range.start,
                        Span::Macro { .. } => 0,
                    };
                    format!("{}:{offset}: {}", file.display(), error.message)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })?;
    parser::parse(stream).map_err(|errors| {
        Error(
            errors
                .into_iter()
                .map(|error| {
                    let offset = match error.spans.first() {
                        Some(Span::Source(range)) => range.start,
                        _ => source.len(),
                    };
                    format!(
                        "{}:{offset}: unexpected {:?}, expected {}",
                        file.display(),
                        error.found,
                        error.expected.join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })
}

fn load_module(
    program: &mut Program,
    package: &str,
    root: &Path,
    file: &Path,
    path: Vec<String>,
    root_depth: usize,
    seen: &mut BTreeSet<(PathBuf, Vec<String>)>,
) -> Result<(), Error> {
    load_source(
        program,
        package,
        sources::Source::File {
            root: root.into(),
            file: file.into(),
        },
        path,
        root_depth,
        seen,
    )
}

fn declaration_source(file: &Path) -> String {
    let quoted = |value: &str| {
        value
            .trim_start()
            .strip_prefix('"')?
            .split_once('"')
            .map(|(field, _)| field.to_owned())
    };
    for root in file.ancestors().skip(1) {
        let Ok(manifest) = fs::read_to_string(root.join("Cargo.toml")) else {
            continue;
        };
        let Some(name) = manifest_field(&manifest, "package", "name").and_then(quoted) else {
            continue;
        };
        let version = manifest_field(&manifest, "package", "version")
            .and_then(quoted)
            .or_else(|| {
                manifest_field(&manifest, "package", "version.workspace")?
                    .trim_start()
                    .starts_with("true")
                    .then_some(())?;
                root.ancestors().find_map(|dir| {
                    let source = fs::read_to_string(dir.join("Cargo.toml")).ok()?;
                    manifest_field(&source, "workspace.package", "version").and_then(quoted)
                })
            })
            .unwrap_or_else(|| "0.0.0".into());
        let relative = file.strip_prefix(root).unwrap_or(file);
        return format!("{name}@{version}:{}", relative.display());
    }
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hash = DefaultHasher::new();
    file.hash(&mut hash);
    format!("standalone:{:016x}", hash.finish())
}

fn load_source(
    program: &mut Program,
    package: &str,
    source: sources::Source,
    path: Vec<String>,
    root_depth: usize,
    seen: &mut BTreeSet<(PathBuf, Vec<String>)>,
) -> Result<(), Error> {
    let (text, file, key) = source.read()?;
    if seen
        .iter()
        .any(|(loaded, parent)| loaded == &key && path.starts_with(parent))
        || !seen.insert((key.clone(), path.clone()))
    {
        return Err(Error(format!(
            "module file loaded twice: {}",
            file.display()
        )));
    }
    let ast = parse_source(&text, &file)?;
    program
        .declaration_sources
        .insert(path.clone(), declaration_source(&key));
    let children = ast
        .declarations
        .iter()
        .filter_map(|d| match &d.value {
            Declaration::Mod {
                name,
                declarations: None,
                ..
            } => Some(name.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    program.modules.push(CompiledModule {
        package: package.into(),
        root_depth,
        path: path.clone(),
        file,
        ast,
    });
    for name in children {
        if path.is_empty() && name == "std" {
            return Err(Error(
                "std is reserved for the embedded standard library".into(),
            ));
        }
        let child = source.child(&name)?;
        let mut child_path = path.clone();
        child_path.push(name);
        load_source(program, package, child, child_path, root_depth, seen)?;
    }
    Ok(())
}

#[derive(Clone)]
struct Entry {
    package: String,
    public: bool,
    module: bool,
}

struct Scope<'a> {
    module: &'a [String],
    root_depth: usize,
    imports: &'a BTreeMap<String, Vec<String>>,
    symbols: &'a BTreeSet<Vec<String>>,
    constructors: &'a BTreeSet<Vec<String>>,
    interfaces: &'a BTreeSet<Vec<String>>,
    resolved: bool,
    exported: &'a BTreeSet<Vec<String>>,
    exports: &'a BTreeMap<Vec<String>, BTreeMap<String, Vec<String>>>,
    references: RefCell<BTreeSet<CdslPath>>,
}

fn resolve_names(program: &mut Program, resolved: bool) -> Result<(), Error> {
    let mut entries = BTreeMap::<Vec<String>, Entry>::new();
    let mut constructors = BTreeSet::new();
    let mut interfaces = BTreeSet::new();
    let mut declarations = BTreeSet::new();
    for module in &program.modules {
        entries.entry(module.path.clone()).or_insert(Entry {
            package: module.package.clone(),
            public: true,
            module: true,
        });
        for declaration in &module.ast.declarations {
            let (path, public, is_module) = match &declaration.value {
                Declaration::Mod { name, public, .. } => (vec![name.clone()], *public, true),
                Declaration::Let { binding, public }
                | Declaration::Interface { binding, public } => {
                    (binding.path.clone(), *public, false)
                }
                Declaration::Enum { name, public, .. }
                | Declaration::Axiom { name, public, .. } => (vec![name.clone()], *public, false),
                Declaration::Use { .. } => continue,
            };
            let mut full = module.path.clone();
            full.extend(path);
            if matches!(declaration.value, Declaration::Interface { .. }) {
                interfaces.insert(full.clone());
            }
            for depth in module.path.len() + 1..full.len() {
                entries.entry(full[..depth].to_vec()).or_insert(Entry {
                    package: module.package.clone(),
                    public,
                    module: true,
                });
            }
            if !declarations.insert(full.clone()) {
                return Err(Error(format!("duplicate definition ::{}", full.join("::"))));
            }
            let previous = entries.insert(
                full.clone(),
                Entry {
                    package: module.package.clone(),
                    public,
                    module: is_module,
                },
            );
            if previous.is_some_and(|entry| {
                !(is_module && entry.module && entry.package == module.package)
            }) {
                return Err(Error(format!("duplicate definition ::{}", full.join("::"))));
            }
            if let Declaration::Enum { variants, .. } = &declaration.value {
                for variant in variants {
                    let mut variant_path = full.clone();
                    variant_path.push(variant.name.clone());
                    constructors.insert(variant_path.clone());
                    if entries
                        .insert(
                            variant_path.clone(),
                            Entry {
                                package: module.package.clone(),
                                public,
                                module: false,
                            },
                        )
                        .is_some()
                    {
                        return Err(Error(format!(
                            "duplicate definition ::{}",
                            variant_path.join("::")
                        )));
                    }
                }
            }
        }
    }
    for module in &program.modules {
        if !entries
            .get(&module.path)
            .is_some_and(|entry| entry.module && entry.package == module.package)
        {
            return Err(Error(format!(
                "module ::{} conflicts with a definition",
                module.path.join("::")
            )));
        }
    }
    let imports = imports::resolve(&program.modules, &entries)?;
    let exports = imports.exports;
    let imports = imports.bindings;
    program.references.clear();
    for module in &mut program.modules {
        let symbols = visible_symbols(&entries, &module.package, &module.path);
        let empty = BTreeMap::new();
        let imported = imports.get(&module.path).unwrap_or(&empty);
        // Canonicalized ASTs retain access through public re-exports, including
        // declarations whose original implementation modules are private.
        let mut exported = BTreeSet::new();
        let mut namespaces = symbols
            .iter()
            .filter(|p| entries[*p].module)
            .cloned()
            .chain(imported.values().cloned())
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        while let Some(target) = namespaces.pop() {
            if !visited.insert(target.clone()) {
                continue;
            }
            if entries[&target].module {
                if let Some(names) = exports.get(&target) {
                    namespaces.extend(names.values().cloned());
                }
            } else {
                exported.insert(target.clone());
                exported.extend(
                    constructors
                        .iter()
                        .filter(|p| p.starts_with(&target))
                        .cloned(),
                );
            }
        }
        let scope = Scope {
            module: &module.path,
            root_depth: module.root_depth,
            imports: imported,
            symbols: &symbols,
            constructors: &constructors,
            interfaces: &interfaces,
            resolved,
            exported: &exported,
            exports: &exports,
            references: RefCell::default(),
        };
        let mut references = Vec::with_capacity(module.ast.declarations.len());
        for declaration in &mut module.ast.declarations {
            let offset = match module.ast.token_spans.get(declaration.range.start) {
                Some(Span::Source(range)) => range.start,
                _ => 0,
            };
            resolve_declaration(declaration, &scope)
                .map_err(|error| Error(format!("{}:{offset}: {error}", module.file.display())))?;
            references.push(scope.references.take());
        }
        program.references.insert(module.path.clone(), references);
    }
    program.module_exports = exports;
    Ok(())
}

fn visible_symbols(
    entries: &BTreeMap<Vec<String>, Entry>,
    package: &str,
    module: &[String],
) -> BTreeSet<Vec<String>> {
    let mut symbols = BTreeSet::new();
    for (path, entry) in entries {
        let visible = (entry.public
            || (entry.package == package
                && module.starts_with(&path[..path.len().saturating_sub(1)])))
            && (1..path.len()).all(|depth| {
                entries.get(&path[..depth]).is_none_or(|parent| {
                    parent.public
                        || (parent.package == package && module.starts_with(&path[..depth - 1]))
                })
            });
        if visible {
            symbols.insert(path.clone());
        }
    }
    symbols
}

fn resolve_path(path: &[String], scope: &Scope<'_>) -> Option<Vec<String>> {
    let resolved =
        if scope.resolved && (scope.symbols.contains(path) || scope.exported.contains(path)) {
            path.to_vec()
        } else {
            resolve_lexical_path(path, scope)?
        };
    scope.references.borrow_mut().insert(resolved.clone());
    Some(resolved)
}

fn resolve_lexical_path(path: &[String], scope: &Scope<'_>) -> Option<Vec<String>> {
    if path.is_empty() {
        return None;
    }
    let mut candidates = Vec::new();
    if let Some(imported) = scope.imports.get(&path[0]).filter(|_| path[0] != "std") {
        candidates.push(imported.clone());
    }
    let depths = if path[0] == "std" {
        0..=0
    } else {
        scope.root_depth..=scope.module.len()
    };
    for depth in depths.rev() {
        let enclosing = &scope.module[..depth];
        let mut candidate = enclosing.to_vec();
        candidate.push(path[0].clone());
        if scope.symbols.contains(&candidate) {
            candidates.push(candidate);
        } else if !crate::infer::builtin_path(path)
            && let Some(target) = scope
                .exports
                .get(enclosing)
                .and_then(|names| names.get(&path[0]))
        {
            candidates.push(target.clone());
        }
    }
    for mut current in candidates {
        let mut valid = true;
        for part in &path[1..] {
            if let Some(target) = scope
                .exports
                .get(&current)
                .and_then(|names| names.get(part))
            {
                current = target.clone();
                continue;
            }
            let mut direct = current.clone();
            direct.push(part.clone());
            if scope.symbols.contains(&direct) || scope.constructors.contains(&direct) {
                current = direct;
            } else {
                valid = false;
                break;
            }
        }
        if valid {
            return Some(current);
        }
    }
    if crate::infer::builtin_path(path) {
        return Some(path.to_vec());
    }
    if path.len() == 1 {
        let mut found = scope.constructors.iter().filter(|p| {
            p.last() == path.last()
                && scope.symbols.contains(*p)
                && p.len() >= 2
                && scope.module.starts_with(&p[..p.len() - 2])
        });
        let first = found.next()?;
        if found.next().is_none() {
            return Some(first.clone());
        }
    }
    None
}

fn resolve_declaration(
    declaration: &mut Node<Declaration>,
    scope: &Scope<'_>,
) -> Result<(), String> {
    match &mut declaration.value {
        Declaration::Use { .. } | Declaration::Mod { .. } => Ok(()),
        Declaration::Let { binding, .. } | Declaration::Interface { binding, .. } => {
            let mut locals = BTreeSet::new();
            for generic in &mut binding.generics {
                resolve_expr(&mut generic.ty, scope, &locals)?;
                locals.insert(generic.name.clone());
            }
            if let Some(ty) = &mut binding.ty {
                resolve_expr(ty, scope, &locals)?;
            }
            resolve_expr(&mut binding.value, scope, &locals)
        }
        Declaration::Enum {
            generics, variants, ..
        } => {
            let mut locals = BTreeSet::new();
            for generic in generics {
                resolve_expr(&mut generic.ty, scope, &locals)?;
                locals.insert(generic.name.clone());
            }
            for variant in variants {
                let mut locals = locals.clone();
                for generic in &mut variant.generics {
                    resolve_expr(&mut generic.ty, scope, &locals)?;
                    locals.insert(generic.name.clone());
                }
                if let Some(arguments) = &mut variant.arguments {
                    for expr in arguments {
                        resolve_expr(expr, scope, &locals)?;
                    }
                }
                if let Some(ty) = &mut variant.ty {
                    resolve_expr(ty, scope, &locals)?;
                }
            }
            Ok(())
        }
        Declaration::Axiom { generics, ty, .. } => {
            let mut locals = BTreeSet::new();
            for generic in generics {
                if let Some(ty) = &mut generic.ty {
                    resolve_expr(ty, scope, &locals)?;
                }
                locals.insert(generic.name.clone());
            }
            resolve_expr(ty, scope, &locals)?;
            Ok(())
        }
    }
}

fn resolve_expr(
    expr: &mut crate::ast::Expr,
    scope: &Scope<'_>,
    locals: &BTreeSet<String>,
) -> Result<(), String> {
    use crate::ast::{Expression, OpField};
    match &mut expr.value {
        Expression::Literal(_) | Expression::Unit => {}
        Expression::Path(path) => {
            if path.len() == 1 && locals.contains(&path[0]) {
                return Ok(());
            }
            *path = resolve_path(path, scope)
                .ok_or_else(|| format!("unresolved name {}", path.join("::")))?;
        }
        Expression::Tuple(values) | Expression::Array(values) => {
            for value in values {
                resolve_expr(value, scope, locals)?;
            }
        }
        Expression::Record(fields) | Expression::Limit(fields) => {
            resolve_record(fields, scope, locals)?
        }
        Expression::Block(block) => resolve_block(block, scope, locals)?,
        Expression::Function(function) => resolve_function(function, scope, locals)?,
        Expression::If {
            condition,
            then,
            otherwise,
        } => {
            resolve_expr(condition, scope, locals)?;
            resolve_block(then, scope, locals)?;
            resolve_expr(otherwise, scope, locals)?;
        }
        Expression::Match { value, arms } => {
            resolve_expr(value, scope, locals)?;
            for (pattern, arm) in arms {
                let mut arm_locals = locals.clone();
                resolve_pattern(pattern, scope, &mut arm_locals)?;
                resolve_expr(arm, scope, &arm_locals)?;
            }
        }
        Expression::Op(fields) => {
            for field in fields {
                if let OpField::Field { name, value } = &mut field.value
                    && name == "interfaces"
                {
                    let Expression::Record(entries) = &mut value.value else {
                        return Err("operation interfaces require a record".into());
                    };
                    field.value = OpField::Interfaces {
                        receiver: "self".into(),
                        entries: std::mem::take(entries)
                            .into_iter()
                            .map(|(name, methods)| (name, Vec::new(), methods))
                            .collect(),
                    };
                }
                match &mut field.value {
                    OpField::Signature {
                        parameters, result, ..
                    } => {
                        let mut local_scope = locals.clone();
                        resolve_binders(parameters, scope, &mut local_scope)?;
                        resolve_expr(result, scope, &local_scope)?;
                    }
                    OpField::Field { value, .. } => resolve_expr(value, scope, locals)?,
                    OpField::Interfaces { receiver, entries } => {
                        let mut attached = BTreeSet::new();
                        let mut method_scope = locals.clone();
                        method_scope.insert(receiver.clone());
                        for (name, declaration, methods) in entries {
                            let path = resolve_lexical_path(std::slice::from_ref(name), scope)
                                .filter(|path| {
                                    !locals.contains(name) && scope.interfaces.contains(path)
                                })
                                .ok_or_else(|| format!("unknown operation interface {name}"))?;
                            if !attached.insert(path.clone()) {
                                return Err(format!("duplicate attachment of {}", path.join("::")));
                            }
                            *declaration = path;
                            resolve_expr(methods, scope, &method_scope)?;
                        }
                    }
                }
            }
        }
        Expression::Ir { bindings, value } => {
            let mut local_scope = locals.clone();
            resolve_bindings(bindings, scope, &mut local_scope)?;
            resolve_expr(value, scope, &local_scope)?;
        }
        Expression::Unary { value, .. } | Expression::Member { value, .. } => {
            resolve_expr(value, scope, locals)?
        }
        Expression::Binary { left, right, .. } => {
            resolve_expr(left, scope, locals)?;
            resolve_expr(right, scope, locals)?;
        }
        Expression::Generic { value, arguments } | Expression::Call { value, arguments } => {
            resolve_expr(value, scope, locals)?;
            for argument in arguments {
                resolve_expr(argument, scope, locals)?;
            }
        }
        Expression::RecordCall {
            value,
            fields,
            arguments,
        } => {
            resolve_expr(value, scope, locals)?;
            resolve_record(fields, scope, locals)?;
            for argument in arguments {
                resolve_expr(argument, scope, locals)?;
            }
        }
    }
    Ok(())
}

fn resolve_record(
    record: &mut crate::ast::Record,
    scope: &Scope<'_>,
    locals: &BTreeSet<String>,
) -> Result<(), String> {
    let mut fields = locals.clone();
    for (name, value) in record {
        resolve_expr(value, scope, &fields)?;
        fields.insert(name.clone());
    }
    Ok(())
}

fn resolve_binders(
    binders: &mut [crate::ast::Binder],
    scope: &Scope<'_>,
    locals: &mut BTreeSet<String>,
) -> Result<(), String> {
    for binder in binders {
        resolve_expr(&mut binder.ty, scope, locals)?;
        locals.insert(binder.name.clone());
    }
    Ok(())
}

fn resolve_bindings(
    bindings: &mut [crate::ast::Binding],
    scope: &Scope<'_>,
    locals: &mut BTreeSet<String>,
) -> Result<(), String> {
    for binding in bindings {
        let mut local_scope = locals.clone();
        resolve_binders(&mut binding.generics, scope, &mut local_scope)?;
        if let Some(ty) = &mut binding.ty {
            resolve_expr(ty, scope, &local_scope)?;
        }
        resolve_expr(&mut binding.value, scope, &local_scope)?;
        if binding.path.len() != 1 {
            return Err("local binding must have one name".into());
        }
        locals.insert(binding.path[0].clone());
    }
    Ok(())
}

fn resolve_block(
    block: &mut crate::ast::Block,
    scope: &Scope<'_>,
    locals: &BTreeSet<String>,
) -> Result<(), String> {
    let mut local_scope = locals.clone();
    resolve_bindings(&mut block.bindings, scope, &mut local_scope)?;
    resolve_expr(&mut block.value, scope, &local_scope)
}

fn resolve_function(
    function: &mut crate::ast::Function,
    scope: &Scope<'_>,
    locals: &BTreeSet<String>,
) -> Result<(), String> {
    let mut local_scope = locals.clone();
    resolve_binders(&mut function.generics, scope, &mut local_scope)?;
    resolve_binders(&mut function.parameters, scope, &mut local_scope)?;
    if let Some(result) = &mut function.result {
        resolve_expr(result, scope, &local_scope)?;
    }
    if let Some(decreases) = &mut function.decreases {
        resolve_expr(decreases, scope, &local_scope)?;
    }
    resolve_block(&mut function.body, scope, &local_scope)
}

fn resolve_pattern(
    pattern: &mut crate::ast::Pattern,
    scope: &Scope<'_>,
    locals: &mut BTreeSet<String>,
) -> Result<(), String> {
    use crate::ast::PatternKind;
    match &mut pattern.value {
        PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::Unit => {}
        PatternKind::Path(path) if path.len() == 1 => {
            if let Some(constructor) =
                resolve_path(path, scope).filter(|p| scope.constructors.contains(p))
            {
                *path = constructor;
            } else {
                if scope
                    .constructors
                    .iter()
                    .filter(|candidate| {
                        candidate.last() == path.last() && scope.symbols.contains(*candidate)
                    })
                    .count()
                    > 1
                {
                    return Err(format!("ambiguous constructor pattern {}", path[0]));
                }
                locals.insert(path[0].clone());
            }
        }
        PatternKind::Path(path) => {
            *path = resolve_path(path, scope)
                .ok_or_else(|| format!("unresolved pattern {}", path.join("::")))?;
        }
        PatternKind::Constructor { path, arguments } => {
            *path = resolve_path(path, scope)
                .ok_or_else(|| format!("unresolved constructor {}", path.join("::")))?;
            for argument in arguments {
                resolve_pattern(argument, scope, locals)?;
            }
        }
        PatternKind::Tuple(values) => {
            for value in values {
                resolve_pattern(value, scope, locals)?;
            }
        }
    }
    Ok(())
}
