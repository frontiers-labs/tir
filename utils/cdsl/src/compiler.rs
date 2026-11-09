//! The shared source-to-checked-module entry point.
use crate::ast::{Item, ItemKind};
use crate::check::Checker;
use crate::core::{Definition, NominalDefinition};
use crate::diagnostic::{Diagnostic, Phase};
use crate::eval::{EvaluationLimits, Evaluator, Value};
use crate::resolve::Symbols;
use crate::types::{Constraint, Type};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Explicit sources and namespace capabilities for one package.
#[derive(Clone, Debug)]
pub struct SourceMap {
    /// Diagnostic name of the root source.
    pub root_name: String,
    /// Stable package identity used by qualified definitions.
    pub package: String,
    /// Root source text.
    pub root: String,
    /// Logical qualified module names mapped to source text.
    pub modules: BTreeMap<String, String>,
    /// Imported nominal namespaces this package may extend.
    pub namespace_capabilities: BTreeSet<String>,
}
impl SourceMap {
    /// Create a package with one root source.
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            root_name: name.into(),
            package: "main".into(),
            root: source.into(),
            modules: BTreeMap::new(),
            namespace_capabilities: BTreeSet::new(),
        }
    }
    /// Load a root and its transitive modules, returning unique canonical dependency paths.
    /// Module files resolve relative to the file containing their declaration.
    pub fn from_file(path: impl AsRef<Path>) -> Result<(Self, Vec<PathBuf>), Vec<Diagnostic>> {
        let path = path.as_ref();
        let canonical = path
            .canonicalize()
            .map_err(|error| file_error(path, error))?;
        let root = std::fs::read_to_string(path).map_err(|error| file_error(path, error))?;
        let mut sources = Self::new(path.display().to_string(), root);
        let items = crate::parser::parse(&sources.root_name, &sources.root)?;
        let mut tracked = BTreeSet::from([canonical.clone()]);
        load_modules(
            &mut sources,
            "",
            path,
            items,
            &mut BTreeSet::from([canonical]),
            &mut tracked,
        )?;
        Ok((sources, tracked.into_iter().collect()))
    }
    /// Supply an out-of-line module. Names are relative to the package root.
    pub fn insert(&mut self, module: impl Into<String>, source: impl Into<String>) {
        self.modules.insert(module.into(), source.into());
    }
}
fn file_error(path: &Path, error: impl std::fmt::Display) -> Vec<Diagnostic> {
    vec![Diagnostic::new(
        &path.display().to_string(),
        0..0,
        Phase::Resolve,
        error.to_string(),
    )]
}

fn load_modules(
    sources: &mut SourceMap,
    module: &str,
    file: &Path,
    items: Vec<Item>,
    active: &mut BTreeSet<PathBuf>,
    tracked: &mut BTreeSet<PathBuf>,
) -> Result<(), Vec<Diagnostic>> {
    for item in items {
        if let ItemKind::Module(name, body) = item.node.kind {
            let key = crate::resolve::qualify(module, &name);
            match body {
                Some(items) => load_modules(sources, &key, file, items, active, tracked)?,
                None => {
                    let base = file
                        .parent()
                        .ok_or_else(|| file_error(file, "source has no parent directory"))?;
                    let path = base.join(format!("{name}.cdsl"));
                    let canonical = path
                        .canonicalize()
                        .map_err(|error| file_error(&path, error))?;
                    if !active.insert(canonical.clone()) {
                        return Err(vec![Diagnostic::new(
                            &path.display().to_string(),
                            item.span,
                            Phase::Resolve,
                            format!("cyclic source module `{key}`"),
                        )]);
                    }
                    tracked.insert(canonical.clone());
                    let text =
                        std::fs::read_to_string(&path).map_err(|error| file_error(&path, error))?;
                    let items = crate::parser::parse(&key, &text)?;
                    sources.insert(&key, text);
                    load_modules(sources, &key, &path, items, active, tracked)?;
                    active.remove(&canonical);
                }
            }
        }
    }
    Ok(())
}

/// A module whose every definition, branch, pattern, and recursive call passed checking.
/// Its constructor is private; unchecked syntax cannot enter evaluation.
pub struct CheckedModule {
    checker: Checker,
}
impl CheckedModule {
    pub(crate) fn inductive_constructors(&self, name: &str) -> Option<&[(String, Vec<Type>)]> {
        self.checker.inductive_constructors(name)
    }

    /// Inspect an externally declared nominal family and its resolved category.
    pub fn nominal_definition(&self, name: &str) -> Option<&NominalDefinition> {
        self.checker.nominals.get(name)
    }

    /// Unify checked bindings with output requirements and resolve their inferred rows.
    pub fn complete(self, requirements: &[(String, Type)]) -> Result<Self, Vec<Diagnostic>> {
        self.checker
            .complete(requirements)
            .map(|checker| Self { checker })
    }

    /// Inspect checked code and its quantified type without admitting unchecked definitions.
    pub fn definition(&self, binding: &str) -> Option<&Definition> {
        self.checker.definitions.get(binding)
    }

    /// Evaluate a checked monomorphic binding by qualified name.
    pub fn evaluate(&self, binding: &str) -> Result<Value, Diagnostic> {
        self.evaluate_with_limits(binding, EvaluationLimits::default())
    }
    /// Evaluate a binding with explicit bounds on work and nesting.
    pub fn evaluate_with_limits(
        &self,
        binding: &str,
        limits: EvaluationLimits,
    ) -> Result<Value, Diagnostic> {
        Evaluator::with_limits(&self.checker.definitions, limits).global(
            binding,
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
    }
    /// Evaluate specification metadata while retaining its rigid generic parameters.
    /// Reading an unspecialized static value still requires concrete specialization.
    pub fn evaluate_template(&self, binding: &str) -> Result<Value, Diagnostic> {
        self.evaluate_template_with_limits(binding, EvaluationLimits::default())
    }
    /// Evaluate generic specification metadata with explicit work and nesting bounds.
    pub fn evaluate_template_with_limits(
        &self,
        binding: &str,
        limits: EvaluationLimits,
    ) -> Result<Value, Diagnostic> {
        let parameters =
            self.checker
                .definitions
                .get(binding)
                .map_or_else(BTreeMap::new, |definition| {
                    definition
                        .scheme
                        .parameters
                        .iter()
                        .map(|(name, kind)| {
                            let argument = if matches!(kind, Type::Universe(_)) {
                                Type::Parameter(name.clone())
                            } else {
                                Type::Index(name.clone(), Box::new(kind.clone()))
                            };
                            (name.clone(), argument)
                        })
                        .collect()
                });
        Evaluator::with_limits(&self.checker.definitions, limits).global(
            binding,
            &parameters,
            &BTreeMap::new(),
        )
    }
    /// Inspect the checked, decoded type of a definition.
    pub fn type_of(&self, binding: &str) -> Option<&Type> {
        self.checker.definitions.get(binding).map(|d| &d.scheme.ty)
    }
    /// Inspect the inferred dictionary requirements of an exported generic definition.
    pub fn constraints(&self, binding: &str) -> Option<&[Constraint]> {
        self.checker
            .definitions
            .get(binding)
            .map(|d| d.scheme.constraints.as_slice())
    }
    /// Iterate checked names and their types in stable order.
    pub fn bindings(&self) -> impl Iterator<Item = (&str, &Type)> {
        self.checker
            .definitions
            .iter()
            .map(|(n, d)| (n.as_str(), &d.scheme.ty))
    }
}
/// Resolve and check every declaration before admitting any evaluation.
pub fn compile(sources: &SourceMap) -> Result<CheckedModule, Vec<Diagnostic>> {
    let symbols = Symbols::load(sources)?;
    let checker = Checker::new(symbols, &sources.package).check()?;
    Ok(CheckedModule { checker })
}

/// Infer every declaration, derive output type requirements, then finish checking.
/// The callback sees read-only inferred code and nominal metadata, without evaluation.
/// Unsolved types and dictionary obligations must be resolved before a module is returned.
pub fn compile_with_requirements(
    sources: &SourceMap,
    derive: impl FnOnce(
        &BTreeMap<String, Definition>,
        &BTreeMap<String, NominalDefinition>,
    ) -> Result<Vec<(String, Type)>, Vec<Diagnostic>>,
) -> Result<CheckedModule, Vec<Diagnostic>> {
    let symbols = Symbols::load(sources)?;
    let checker = Checker::new(symbols, &sources.package).infer_definitions()?;
    let requirements = derive(&checker.definitions, &checker.nominals)?;
    let checker = checker.complete(&requirements)?;
    Ok(CheckedModule { checker })
}
