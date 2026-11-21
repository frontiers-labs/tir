mod coverage;
pub(crate) mod sugar;
mod terms;
mod types;

// Elaboration of resolved CDSL expressions into Hindley-Milner constraints.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    CheckedMethods,
    ast::*,
    compiler::Program,
    error::Error,
    hm::{self, ConId, RigidId, Scheme, Substitution, Ty, TyList},
};

type Definitions<'a> = BTreeMap<Path, &'a Binding>;

enum FunctionMode<'a> {
    Infer(Option<&'a Path>),
    Check(&'a Ty),
}
type Environment = BTreeMap<Path, BoundValue>;
#[derive(Clone)]
struct BoundValue {
    scheme: Scheme,
}
impl BoundValue {
    fn mono(ty: Ty) -> Self {
        Scheme::mono(ty).into()
    }
}
impl From<Scheme> for BoundValue {
    fn from(scheme: Scheme) -> Self {
        Self { scheme }
    }
}
impl std::ops::Deref for BoundValue {
    type Target = Scheme;
    fn deref(&self) -> &Scheme {
        &self.scheme
    }
}

// None records a value binder that shadows a type name.
type Types = BTreeMap<Path, Option<Ty>>;
type MatchArguments = BTreeMap<usize, [Ty; 2]>;

const TYPE: ConId = ConId(0);
const NAT: ConId = ConId(1);
const BOOL: ConId = ConId(2);
const STRING: ConId = ConId(3);
pub(crate) const UNIT: ConId = ConId(4);
pub(crate) const TUPLE: ConId = ConId(5);
const VECTOR: ConId = ConId(10);
const SUM: ConId = ConId(15);
const PRODUCT: ConId = ConId(16);
const TYPE1: ConId = ConId(18);
const FACT: ConId = ConId(32);
const INT: ConId = ConId(41);

pub(crate) fn builtin_path(path: &[String]) -> bool {
    matches!(
        path.join("::").as_str(),
        "Type" | "Prop" | "Nat" | "Int" | "Bool" | "String" | "lt" | "gt"
    )
}

fn lookup_type(local: &Types, global: &Types, path: &Path) -> Option<Option<Ty>> {
    match local.get(path) {
        Some(ty) => Some(ty.clone()),
        None => global.get(path).cloned(),
    }
}

fn con(id: ConId) -> Ty {
    Ty::Con(id, Box::new(TyList::Nil))
}

/// Infer every declaration, including dependencies, after name resolution.
pub fn check(program: &Program) -> Result<BTreeMap<Path, Scheme>, Error> {
    let checker = elaborate(program)?;
    Ok(checker
        .environment
        .iter()
        .map(|(path, scheme)| (path.clone(), checker.substitution.apply_scheme(scheme)))
        .collect())
}
pub(crate) fn lower(program: &Program) -> Result<crate::CheckedProgram, Error> {
    let checker = elaborate(program)?;
    let types = checker
        .environment
        .iter()
        .map(|(path, scheme)| (path.clone(), checker.substitution.apply_scheme(scheme)))
        .collect();
    let mut operations = checker.checked_interfaces;
    for methods in operations.values_mut().flat_map(BTreeMap::values_mut) {
        methods.map_types(|ty| checker.substitution.apply(ty));
    }
    crate::CheckedProgram::new(
        program.clone(),
        types,
        operations,
        checker
            .types
            .into_iter()
            .filter_map(|(path, ty)| match ty {
                Some(Ty::Con(id, _)) => Some((path, id)),
                _ => None,
            })
            .collect(),
        checker.operation_matches,
    )
}
fn elaborate(program: &Program) -> Result<Checker, Error> {
    crate::totality::check(program)?;
    let mut definitions = BTreeMap::new();
    for module in program.all_modules() {
        for declaration in &module.ast.declarations {
            if let Declaration::Let { binding, .. } | Declaration::Interface { binding, .. } =
                &declaration.value
            {
                let mut path = module.path.clone();
                path.extend(binding.path.clone());
                definitions.insert(path, binding);
            }
        }
    }
    let mut checker = Checker {
        next: 0,
        rigid: 0,
        substitution: Substitution::default(),
        environment: BTreeMap::new(),
        types: BTreeMap::new(),
        constructors: BTreeSet::new(),
        interfaces: BTreeSet::new(),
        checked_interfaces: BTreeMap::new(),
        operation_kinds: BTreeMap::new(),
        operation_matches: BTreeMap::new(),
        locations: BTreeMap::new(),
        arities: BTreeMap::new(),
        pending: Vec::new(),
        frames: Vec::new(),
        roots: BTreeMap::new(),
        explicit: BTreeMap::new(),
        records: Vec::new(),
        next_con: 64,
        naturals: BTreeMap::new(),
        rigid_types: BTreeMap::new(),
        index_types: BTreeMap::new(),
        dependent_records: BTreeMap::new(),
        dependent_variables: BTreeSet::new(),
        axioms: BTreeMap::new(),
        checking_axioms: BTreeSet::new(),
        declared: BTreeMap::new(),
        indexed_terms: Vec::new(),
        term_parameters: BTreeMap::new(),
        relations: BTreeSet::new(),
        inductive_ids: BTreeSet::new(),
        inductive_edges: Vec::new(),
        module_exports: program.module_exports.clone(),
        checking_modules: BTreeSet::new(),
    };
    for (name, id) in [
        ("Prop", FACT),
        ("Type", TYPE),
        ("Nat", NAT),
        ("Int", INT),
        ("Bool", BOOL),
        ("String", STRING),
    ] {
        checker.types.insert(vec![name.into()], Some(con(id)));
        checker
            .environment
            .insert(vec![name.into()], BoundValue::mono(con(TYPE)));
    }
    checker
        .environment
        .insert(vec!["Type".into()], BoundValue::mono(con(TYPE1)));
    for comparison in ["lt", "gt"] {
        checker.environment.insert(
            vec![comparison.into()],
            BoundValue::mono(Ty::Fun(
                type_list(vec![con(NAT), con(NAT)]),
                Box::new(con(BOOL)),
            )),
        );
    }
    checker = checker.declare_modules(program.all_modules())?;
    for module in program.all_modules() {
        for declaration in &module.ast.declarations {
            if let Declaration::Enum { name, generics, .. } = &declaration.value {
                let mut path = module.path.clone();
                path.push(name.clone());
                let Some(Some(Ty::Con(_, parameters))) = checker.types.get(&path).cloned() else {
                    return Err(Error("enum declaration has no type constructor".into()));
                };
                let mut types = checker.types.clone();
                for (parameter, binder) in list_values(&parameters).into_iter().zip(generics) {
                    let kind = checker.parameter_type(&definitions, binder, &types)?;
                    if let Ty::Var(id) = parameter {
                        checker.index_types.insert(id.0, kind);
                    }
                    types.insert(vec![binder.name.clone()], Some(parameter));
                }
            }
        }
    }
    checker = checker.variant_modules(&definitions, program.all_modules())?;
    checker.check_inductive_cycles()?;
    for clause in [false, true] {
        for (path, (generics, annotation)) in checker.axioms.clone() {
            if clause_annotation(&annotation) == clause {
                checker
                    .axiom(&definitions, &path, &generics, &annotation)
                    .map_err(|error| Error(format!("{}: {error}", path.join("::"))))?;
            }
        }
    }
    let mut outcome = Ok(());
    for path in definitions.keys() {
        if let Err(error) = checker.definition(&definitions, path) {
            outcome = Err(error);
            break;
        }
    }
    outcome?;
    for (path, value) in &checker.environment {
        if any_rigid(&checker.substitution.apply(&value.ty)) {
            return Err(Error(format!(
                "{} has an unbound dependent/contextual obligation; explicit evidence is required",
                path.join("::")
            )));
        }
    }
    Ok(checker)
}

struct Checker {
    next: usize,
    rigid: usize,
    substitution: Substitution,
    environment: Environment,
    types: Types,
    constructors: BTreeSet<Path>,
    interfaces: BTreeSet<Path>,
    checked_interfaces: BTreeMap<Path, BTreeMap<Path, CheckedMethods>>,
    operation_kinds: BTreeMap<Path, Scheme>,
    operation_matches: BTreeMap<Path, BTreeMap<usize, crate::lower::CheckedMatch>>,
    locations: BTreeMap<Path, String>,
    arities: BTreeMap<Path, usize>,
    pending: Vec<Path>,
    frames: Vec<Path>,
    roots: BTreeMap<Path, usize>,
    explicit: BTreeMap<Path, Vec<Ty>>,
    records: Vec<(Vec<String>, ConId)>,
    next_con: usize,
    naturals: BTreeMap<u128, ConId>,
    rigid_types: BTreeMap<usize, Ty>,
    index_types: BTreeMap<usize, Ty>,
    dependent_records: BTreeMap<usize, Vec<(usize, Ty)>>,
    dependent_variables: BTreeSet<hm::VarId>,
    axioms: BTreeMap<Path, (Vec<RuleBinder>, Expr)>,
    checking_axioms: BTreeSet<Path>,
    declared: BTreeMap<Path, (ConId, Vec<Ty>, Ty)>,
    indexed_terms: Vec<(ConId, Expr, Ty)>,
    term_parameters: BTreeMap<usize, Vec<Path>>,
    relations: BTreeSet<Path>,
    inductive_ids: BTreeSet<usize>,
    inductive_edges: Vec<(usize, usize, bool)>,
    module_exports: BTreeMap<Path, BTreeMap<String, Path>>,
    checking_modules: BTreeSet<Path>,
}

impl Checker {
    fn declare(
        &mut self,
        module: &crate::compiler::CompiledModule,
        declaration: &Node<Declaration>,
    ) -> Result<(), Error> {
        match &declaration.value {
            Declaration::Let { binding, .. } | Declaration::Interface { binding, .. } => {
                let mut path = module.path.clone();
                path.extend(binding.path.clone());
                if self.environment.contains_key(&path) || builtin_path(&path) {
                    return Err(Error("HM: reserved builtin name".into()));
                }
                if matches!(declaration.value, Declaration::Interface { .. }) {
                    self.interfaces.insert(path.clone());
                }
                self.locations
                    .insert(path.clone(), declaration_location(module, declaration));
            }
            Declaration::Enum { name, generics, .. } => {
                let mut path = module.path.clone();
                path.push(name.clone());
                if self.environment.contains_key(&path) || builtin_path(&path) {
                    return Err(Error("HM: reserved builtin name".into()));
                }
                let id = self.constructor()?;
                self.inductive_ids.insert(id.0);
                let args = self.fresh_types(generics.len())?;
                self.arities.insert(path.clone(), generics.len());
                self.types
                    .insert(path.clone(), Some(Ty::Con(id, type_list(args))));
                self.environment.insert(path, BoundValue::mono(con(TYPE)));
            }
            Declaration::Axiom {
                name, generics, ty, ..
            } => {
                let mut path = module.path.clone();
                path.push(name.clone());
                if builtin_path(&path) || self.environment.contains_key(&path) {
                    return Err(Error("reserved builtin name".into()));
                }
                if matches!(&ty.value, Expression::Path(name) if name == &vec!["Type".to_owned()]) {
                    let id = self.constructor()?;
                    self.types.insert(path.clone(), Some(con(id)));
                    self.arities.insert(path.clone(), 0);
                }
                self.axioms.insert(path, (generics.clone(), ty.clone()));
            }
            Declaration::Use { .. } | Declaration::Mod { .. } => {}
        }
        Ok(())
    }
    fn variants(
        &mut self,
        definitions: &Definitions<'_>,
        module: &crate::compiler::CompiledModule,
        declaration: &Node<Declaration>,
    ) -> Result<(), Error> {
        if let Declaration::Enum {
            name,
            generics,
            variants,
            ..
        } = &declaration.value
        {
            let mut path = module.path.clone();
            path.push(name.clone());
            let mut types = self.types.clone();
            let mut quantified = Vec::new();
            let mut parameters = Vec::new();
            let mut names = BTreeSet::new();
            let mut outcome = Ok(());
            for binder in generics {
                match self.enum_parameter(definitions, binder, &types, &mut names) {
                    Ok(parameter) => {
                        if let Ty::Var(id) = parameter {
                            quantified.push(id);
                        }
                        types.insert(vec![binder.name.clone()], Some(parameter.clone()));
                        parameters.push(parameter);
                    }
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }
            outcome?;
            let Some(Some(Ty::Con(id, _))) = self.types.get(&path).cloned() else {
                return Err(Error("HM: missing enum type".into()));
            };
            let result = Ty::Con(id, type_list(parameters.clone()));
            self.types.insert(path.clone(), Some(result.clone()));
            types.insert(path.clone(), Some(result.clone()));
            let mut outcome = Ok(());
            for variant in variants {
                outcome = self.variant(
                    definitions,
                    variant,
                    &path,
                    &types,
                    (&parameters, &quantified),
                    &result,
                );
                if outcome.is_err() {
                    break;
                }
            }
            outcome?;
        }
        Ok(())
    }
    fn declare_modules<'a>(
        self,
        modules: impl IntoIterator<Item = &'a crate::compiler::CompiledModule>,
    ) -> Result<Self, Error> {
        modules.into_iter().try_fold(self, |checker, module| {
            checker.declare_declarations(module, &module.ast.declarations)
        })
    }
    fn declare_declarations(
        self,
        module: &crate::compiler::CompiledModule,
        declarations: &[Node<Declaration>],
    ) -> Result<Self, Error> {
        declarations
            .iter()
            .try_fold(self, |mut checker, declaration| {
                if let Err(error) = checker.declare(module, declaration) {
                    return Err(declaration_error(module, declaration, error));
                }
                Ok(checker)
            })
    }

    fn variant_modules<'a>(
        self,
        definitions: &Definitions<'_>,
        modules: impl IntoIterator<Item = &'a crate::compiler::CompiledModule>,
    ) -> Result<Self, Error> {
        modules.into_iter().try_fold(self, |checker, module| {
            checker.variant_declarations(definitions, module, &module.ast.declarations)
        })
    }
    fn variant_declarations(
        self,
        definitions: &Definitions<'_>,
        module: &crate::compiler::CompiledModule,
        declarations: &[Node<Declaration>],
    ) -> Result<Self, Error> {
        declarations
            .iter()
            .try_fold(self, |mut checker, declaration| {
                if let Err(error) = checker.variants(definitions, module, declaration) {
                    return Err(declaration_error(module, declaration, error));
                }
                Ok(checker)
            })
    }

    fn fresh_types(&mut self, count: usize) -> Result<Vec<Ty>, Error> {
        let mut result = Vec::new();
        let mut index = 0;
        loop {
            if index == count {
                break Ok(result);
            }
            match self.fresh() {
                Ok(value) => {
                    result.push(value);
                    index += 1;
                }
                Err(error) => break Err(error),
            }
        }
    }
    fn annotations(
        &mut self,
        definitions: &Definitions<'_>,
        items: &[Expr],
        types: &Types,
    ) -> Result<Vec<Ty>, Error> {
        let mut result = Vec::new();
        let mut index = 0;
        loop {
            if index == items.len() {
                break Ok(result);
            }
            match self.annotation(definitions, &items[index], types) {
                Ok(value) => {
                    result.push(value);
                    index += 1;
                }
                Err(error) => break Err(error),
            }
        }
    }
    fn expressions(
        &mut self,
        definitions: &Definitions<'_>,
        items: &[Expr],
        environment: &Environment,
        types: &Types,
    ) -> Result<Vec<Ty>, Error> {
        let mut result = Vec::new();
        let mut index = 0;
        loop {
            if index == items.len() {
                break Ok(result);
            }
            match self.expression(definitions, &items[index], environment, types) {
                Ok(value) => {
                    result.push(value);
                    index += 1;
                }
                Err(error) => break Err(error),
            }
        }
    }
    fn enum_parameter(
        &mut self,
        definitions: &Definitions<'_>,
        binder: &Binder,
        types: &Types,
        names: &mut BTreeSet<String>,
    ) -> Result<Ty, Error> {
        if !names.insert(binder.name.clone()) {
            return Err(Error("HM: duplicate enum parameter".into()));
        }
        let kind = self.parameter_type(definitions, binder, types)?;
        let ty = self.fresh()?;
        if let Ty::Var(id) = ty {
            self.index_types.insert(id.0, kind);
        }
        Ok(ty)
    }
    fn variant(
        &mut self,
        definitions: &Definitions<'_>,
        variant: &Variant,
        path: &Path,
        types: &Types,
        generics: (&[Ty], &[hm::VarId]),
        result: &Ty,
    ) -> Result<(), Error> {
        let (parameters, _quantified) = generics;
        let mut local = types.clone();
        let mut names = BTreeSet::new();
        let mut variant_parameters = Vec::new();
        for binder in &variant.generics {
            let parameter = self.enum_parameter(definitions, binder, &local, &mut names)?;
            local.insert(vec![binder.name.clone()], Some(parameter.clone()));
            variant_parameters.push(parameter);
        }
        let output = if let Some(annotation) = &variant.ty {
            self.annotation(definitions, annotation, &local)?
        } else {
            result.clone()
        };
        if !matches!((&output,result),(Ty::Con(a,_),Ty::Con(b,_)) if a==b) {
            return Err(Error(
                "constructor must return its declared inductive family".into(),
            ));
        }
        let result_variables = hm::free_variables(&output);
        for parameter in &variant_parameters {
            if let Ty::Var(id) = parameter
                && !result_variables.contains(id)
            {
                return Err(Error("existential constructor needs an elimination proof; its generic is absent from the result".into()));
            }
        }
        let ty = match &variant.arguments {
            Some(arguments) => {
                let args = self.annotations(definitions, arguments, &local)?;
                if let Ty::Con(family, _) = result {
                    for argument in &args {
                        check_positive(argument, *family)?;
                        self.inductive_occurrences(*family, argument, true);
                    }
                }
                Ty::Fun(type_list(args), Box::new(output))
            }
            None => output,
        };
        let quantified = hm::free_variables(&ty);
        let explicit = parameters
            .iter()
            .chain(&variant_parameters)
            .filter(|p| matches!(p,Ty::Var(id) if quantified.contains(id)))
            .cloned()
            .collect::<Vec<_>>();
        let mut variant_path = path.clone();
        variant_path.push(variant.name.clone());
        self.constructors.insert(variant_path.clone());
        if !explicit.is_empty() {
            self.explicit.insert(variant_path.clone(), explicit);
        }
        self.environment
            .insert(variant_path, Scheme { quantified, ty }.into());
        Ok(())
    }
    fn generic_parameter(
        &mut self,
        definitions: &Definitions<'_>,
        binder: &Binder,
        types: &Types,
        previous: &[Binder],
    ) -> Result<RigidId, Error> {
        let kind = self.parameter_type(definitions, binder, types)?;
        let mut duplicate = false;
        for other in previous {
            if other.name == binder.name {
                duplicate = true;
                break;
            }
        }
        if duplicate {
            return Err(Error("HM: duplicate generic binder".into()));
        }
        let id = RigidId(self.rigid);
        self.rigid = self
            .rigid
            .checked_add(1)
            .ok_or_else(|| Error("HM: rigid identities exhausted".into()))?;
        self.rigid_types.insert(id.0, kind);
        Ok(id)
    }
    fn local_binding(
        &mut self,
        definitions: &Definitions<'_>,
        binding: &Binding,
        local: &mut Environment,
        types: &mut Types,
    ) -> Result<(), Error> {
        let ty = self.binding(definitions, binding, local, types, None)?;
        let mut alias = None;
        if binding.generics.is_empty() && ty == con(TYPE) {
            match self.annotation(definitions, &binding.value, types) {
                Ok(value) => alias = Some(value),
                Err(error) => {
                    let opaque = crate::totality::free_paths(&binding.value)?
                        .iter()
                        .any(|path| types.get(path) != self.types.get(path));
                    if !opaque {
                        return Err(error);
                    }
                    let id = RigidId(self.rigid);
                    self.rigid = self
                        .rigid
                        .checked_add(1)
                        .ok_or_else(|| Error("rigid identities exhausted".into()))?;
                    self.rigid_types.insert(id.0, con(TYPE));
                    alias = Some(Ty::Rigid(id));
                }
            }
        }
        if ty == con(NAT) {
            // A failed optional fold leaves a neutral term, never a fallback to a shadowed global.
            if let Ok(index) = self.index(definitions, &binding.value, types) {
                alias = Some(index);
            }
        }
        let mut outer = self.environment.clone();
        outer.extend(local.clone());
        let scheme = self.generalize(&outer, &ty);
        local.insert(binding.path.clone(), scheme.into());
        types.insert(binding.path.clone(), alias);
        Ok(())
    }
    fn array_item(
        &mut self,
        definitions: &Definitions<'_>,
        item: &Expr,
        element: &Ty,
        environment: &Environment,
        types: &Types,
    ) -> Result<(), Error> {
        let ty = self.expression(definitions, item, environment, types)?;
        self.unify(element, &ty)
    }
    fn match_arm(
        &mut self,
        definitions: &Definitions<'_>,
        arm: &(Pattern, Expr),
        input: &Ty,
        output: &Ty,
        environment: &Environment,
        types: &Types,
    ) -> Result<(), Error> {
        let (pattern, arm) = arm;
        let mut local = environment.clone();
        let mut bound = Vec::new();
        self.pattern(definitions, pattern, input, &mut local, &mut bound)?;
        let mut arm_types = types.clone();
        for name in bound {
            arm_types.insert(vec![name], None);
        }
        let ty = self.expression(definitions, arm, &local, &arm_types)?;
        self.unify(output, &ty)
    }
    fn type_argument(
        &mut self,
        definitions: &Definitions<'_>,
        parameter: &Ty,
        argument: &Expr,
        types: &Types,
        supplied: &Substitution,
    ) -> Result<(hm::VarId, Ty), Error> {
        let Ty::Var(id) = self.substitution.apply(parameter) else {
            return Err(Error("HM: universal parameter lost its identity".into()));
        };
        let kind = supplied.apply(&self.index_types.get(&id.0).cloned().unwrap_or(con(TYPE)));
        Ok((
            id,
            self.parameter_argument(definitions, argument, types, kind)?,
        ))
    }
    fn fresh(&mut self) -> Result<Ty, Error> {
        hm::fresh(&mut self.next).map_err(kernel_error)
    }
    fn constructor(&mut self) -> Result<ConId, Error> {
        let id = self.next_con;
        self.next_con = id
            .checked_add(1)
            .ok_or_else(|| Error("HM: constructor identities exhausted".into()))?;
        Ok(ConId(id))
    }
    fn unify(&mut self, left: &Ty, right: &Ty) -> Result<(), Error> {
        let left = self.normalize_indices(&self.substitution.apply(left))?;
        let right = self.normalize_indices(&self.substitution.apply(right))?;
        let next = hm::unify(&left, &right).map_err(kernel_error)?;
        self.substitution = next.compose(&self.substitution);
        Ok(())
    }
    fn instantiate(&mut self, scheme: &Scheme) -> Result<Ty, Error> {
        let start = self.next;
        let result = hm::instantiate(&self.substitution.apply_scheme(scheme), &mut self.next)
            .map_err(kernel_error)?;
        let renaming = Substitution {
            bindings: scheme
                .quantified
                .iter()
                .enumerate()
                .map(|(offset, original)| (*original, Ty::Var(hm::VarId(start + offset))))
                .collect(),
        };
        for (offset, original) in scheme.quantified.iter().enumerate() {
            let fresh = hm::VarId(start + offset);
            if let Some(kind) = self.index_types.get(&original.0).cloned() {
                self.index_types.insert(fresh.0, renaming.apply(&kind));
            }
            if self.dependent_variables.contains(original) {
                self.dependent_variables.insert(fresh);
            }
        }
        Ok(result)
    }
    fn generalize(&self, environment: &Environment, ty: &Ty) -> Scheme {
        let ty = self.substitution.apply(ty);
        if hm::free_variables(&ty).is_empty() {
            return Scheme::mono(ty);
        }
        let mut schemes = Vec::new();
        for scheme in environment.values() {
            schemes.push(self.substitution.apply_scheme(scheme));
        }
        hm::generalize(&schemes, &ty)
    }
    fn definition(&mut self, definitions: &Definitions<'_>, path: &Path) -> Result<Ty, Error> {
        self.definition_inner(definitions, path)
            .map_err(|error| match self.locations.get(path) {
                Some(location) => Error(format!("{location}: {error}")),
                None => error,
            })
    }
    fn definition_inner(
        &mut self,
        definitions: &Definitions<'_>,
        path: &Path,
    ) -> Result<Ty, Error> {
        if !self.environment.contains_key(path)
            && let Some((generics, annotation)) = self.axioms.get(path).cloned()
        {
            self.axiom(definitions, path, &generics, &annotation)?;
        }
        if let Some(scheme) = self.environment.get(path).cloned() {
            if let Some(root) = self.roots.get(path).copied()
                && let Some(frame) = self.frames.last()
                && let Some(current) = self.roots.get_mut(frame)
            {
                *current = (*current).min(root);
            }
            return self.instantiate(&scheme);
        }
        let binding = definitions
            .get(path)
            .copied()
            .ok_or_else(|| Error(format!("HM: unknown value {}", path.join("::"))))?;
        let index = self.pending.len();
        self.pending.push(path.clone());
        self.frames.push(path.clone());
        self.roots.insert(path.clone(), index);
        let placeholder = self.fresh()?;
        self.environment
            .insert(path.clone(), BoundValue::mono(placeholder.clone()));
        let environment = Environment::new();
        let types = self.types.clone();
        let implicit_alias = binding.ty.is_none()
            && binding.generics.is_empty()
            && self.syntactic_type(&binding.value, definitions, &types);
        let ty = if self.interfaces.contains(path) {
            let (types, generics) = self.generics(definitions, &binding.generics, &types)?;
            let target = self.annotation(definitions, &binding.value, &types)?;
            if self.universe_level(&target) != 0 {
                return Err(Error("interface contract must be a small Type".into()));
            }
            if generics.is_empty() {
                self.types.insert(path.clone(), Some(target));
            }
            self.discharge(&con(TYPE), &generics, &environment, Some(path))?
        } else if implicit_alias {
            let target=self.denotation(definitions,&binding.value,&types)?;
            let sort=self.value_sort(&target);
            self.types.insert(path.clone(),Some(target));
            sort
        } else if binding.generics.is_empty() && binding.ty.as_ref().is_some_and(|annotation| {
            matches!(&annotation.value, Expression::Path(name) if types.get(name) == Some(&Some(con(TYPE))))
        }) {
            let target = self.annotation(definitions, &binding.value, &types)?;
            if self.universe_level(&target)>0 {return Err(Error("universe mismatch: Type does not contain itself".into()));}
            self.types.insert(path.clone(), Some(target));
            con(TYPE)
        } else {
            self.binding(definitions, binding, &environment, &types, Some(path))
                .map_err(|error| Error(format!("{}: {error}", path.join("::"))))?
        };
        self.unify(&placeholder, &ty)?;
        if binding.generics.is_empty()
            && matches!(self.substitution.apply(&ty), Ty::Con(TYPE, _))
            && !self.types.contains_key(path)
        {
            let target = self.annotation(definitions, &binding.value, &types)?;
            if self.universe_level(&target) > 0 {
                return Err(Error("universe mismatch in computed type value".into()));
            }
            self.types.insert(path.clone(), Some(target));
        }
        self.frames.pop();
        let root = self
            .roots
            .get(path)
            .copied()
            .ok_or_else(|| Error("HM: missing inference group".into()))?;
        if let Some(frame) = self.frames.last()
            && let Some(parent) = self.roots.get_mut(frame)
        {
            *parent = (*parent).min(root);
        }
        if root == index {
            let group = self.pending.split_off(index);
            let members = self.take_group(&group)?;
            let mut schemes = Vec::new();
            for (name, ty) in members {
                schemes.push((name, self.generalize(&self.environment, &ty)));
            }
            for (name, scheme) in schemes {
                self.environment.insert(name, scheme.into());
            }
        }
        let scheme = self
            .environment
            .get(path)
            .cloned()
            .ok_or_else(|| Error("HM: missing inferred definition".into()))?;
        self.instantiate(&scheme)
    }
    fn take_group(&mut self, group: &[Path]) -> Result<Vec<(Path, Ty)>, Error> {
        let mut members = Vec::new();
        let mut index = 0;
        loop {
            if index == group.len() {
                break Ok(members);
            }
            let name = &group[index];
            match self.environment.remove(name) {
                Some(entry) => {
                    members.push((name.clone(), self.substitution.apply(&entry.ty)));
                    self.roots.remove(name);
                    index += 1;
                }
                None => break Err(Error("HM: missing recursive placeholder".into())),
            }
        }
    }
    fn lookup(
        &mut self,
        definitions: &Definitions<'_>,
        path: &Path,
        environment: &Environment,
    ) -> Result<Ty, Error> {
        if let Some(scheme) = environment.get(path) {
            return self.instantiate(scheme);
        }
        if let Some(exports) = self.module_exports.get(path).cloned() {
            if !self.checking_modules.insert(path.clone()) {
                return Err(Error("cyclic module value".into()));
            }
            let fields = exports
                .into_iter()
                .map(|(name, member)| {
                    self.lookup(definitions, &member, environment)
                        .map(|ty| (name, ty))
                })
                .collect::<Result<Vec<_>, _>>();
            self.checking_modules.remove(path);
            return self.record_type(fields?, Vec::new());
        }
        self.definition(definitions, path)
    }
    fn parameter_type(
        &mut self,
        definitions: &Definitions<'_>,
        binder: &Binder,
        types: &Types,
    ) -> Result<Ty, Error> {
        self.annotation(definitions, &binder.ty, types)
    }
    fn generics(
        &mut self,
        definitions: &Definitions<'_>,
        binders: &[Binder],
        types: &Types,
    ) -> Result<(Types, Vec<RigidId>), Error> {
        let mut result = types.clone();
        let mut ids = Vec::new();
        let mut outcome = Ok(());
        for binder in binders {
            match self.generic_parameter(definitions, binder, &result, &binders[..ids.len()]) {
                Ok(id) => {
                    ids.push(id);
                    result.insert(vec![binder.name.clone()], Some(Ty::Rigid(id)));
                }
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        outcome?;
        Ok((result, ids))
    }
    fn discharge(
        &mut self,
        ty: &Ty,
        ids: &[RigidId],
        environment: &Environment,
        owner: Option<&Path>,
    ) -> Result<Ty, Error> {
        if ids.is_empty() {
            return Ok(self.substitution.apply(ty));
        }
        let mut outcome = Ok(());
        for (path, scheme) in environment.iter().chain(self.environment.iter()) {
            if owner == Some(path) {
                continue;
            }
            let resolved = self.substitution.apply_scheme(scheme);
            if contains_rigid(&resolved.ty, ids) {
                outcome = Err(Error(
                    "HM: universal type escapes into its environment".into(),
                ));
                break;
            }
        }
        outcome?;
        let fresh = self.fresh_types(ids.len())?;
        let mut replacements = Vec::new();
        for (id, ty) in ids.iter().zip(fresh) {
            replacements.push((*id, ty));
        }
        for (id, ty) in &replacements {
            if let Ty::Var(variable) = ty
                && let Some(kind) = self.rigid_types.get(&id.0)
            {
                self.index_types
                    .insert(variable.0, replace_rigid(kind, &replacements));
            }
        }
        let result = replace_rigid(&self.substitution.apply(ty), &replacements);
        for (_, value) in &mut self.substitution.bindings {
            *value = replace_rigid(value, &replacements);
        }
        if let Some(path) = owner {
            if self.explicit.contains_key(path) {
                return Err(Error(
                    "HM: combined binding and function universals are unsupported".into(),
                ));
            }
            if let Some(methods) = self.checked_interfaces.get_mut(path) {
                for implementation in methods.values_mut() {
                    implementation.map_types(|ty| replace_rigid(ty, &replacements));
                }
            }
            self.explicit.insert(
                path.clone(),
                replacements.into_iter().map(|(_, ty)| ty).collect(),
            );
        }
        Ok(result)
    }
    fn binding(
        &mut self,
        definitions: &Definitions<'_>,
        binding: &Binding,
        environment: &Environment,
        types: &Types,
        owner: Option<&Path>,
    ) -> Result<Ty, Error> {
        let (types, ids) = self.generics(definitions, &binding.generics, types)?;
        let mut local = environment.clone();
        for (binder, id) in binding.generics.iter().zip(&ids) {
            let kind = self
                .rigid_types
                .get(&id.0)
                .ok_or_else(|| Error("generic type was not elaborated".into()))?
                .clone();
            local.insert(vec![binder.name.clone()], BoundValue::mono(kind));
        }
        if let (Some(path), Some(annotation)) = (owner, &binding.ty) {
            let expected = self.annotation(definitions, annotation, &types)?;
            let recursive = self
                .environment
                .get(path)
                .cloned()
                .ok_or_else(|| Error("HM: missing recursive annotation".into()))?;
            self.unify(&recursive.ty, &expected)?;
        }
        let ty = if let Expression::Function(function) = &binding.value.value {
            self.function(
                definitions,
                function,
                &local,
                &types,
                FunctionMode::Infer(owner),
            )?
        } else {
            if let Some(annotation) = &binding.ty {
                let expected = self.annotation(definitions, annotation, &types)?;
                self.check_expression(definitions, &binding.value, &expected, &local, &types)?
            } else {
                self.expression(definitions, &binding.value, &local, &types)?
            }
        };
        if let Some(annotation) = &binding.ty {
            let expected = self.annotation(definitions, annotation, &types)?;
            self.unify(&ty, &expected)?;
        }
        self.discharge(&ty, &ids, environment, owner)
    }
    fn check_expression(
        &mut self,
        definitions: &Definitions<'_>,
        expr: &Expr,
        expected: &Ty,
        environment: &Environment,
        types: &Types,
    ) -> Result<Ty, Error> {
        let expected = self.resolve_computation(definitions, &self.substitution.apply(expected))?;
        if let Expression::Op(fields) = &expr.value {
            return self.operation(
                definitions,
                expr,
                fields,
                Some(&expected),
                environment,
                types,
            );
        }
        if let Expression::Limit(fields) = &expr.value {
            let record = Node {
                value: Expression::Record(fields.clone()),
                range: expr.range.clone(),
            };
            return self.check_expression(definitions, &record, &expected, environment, types);
        }
        if let Expression::Call { value, arguments } = &expr.value {
            return self.call(
                definitions,
                value,
                arguments,
                environment,
                types,
                Some(&expected),
            );
        }
        if let Expression::RecordCall {
            value,
            fields,
            arguments,
        } = &expr.value
        {
            let record = Node {
                value: Expression::Record(fields.clone()),
                range: expr.range.clone(),
            };
            let mut supplied = vec![record];
            supplied.extend_from_slice(arguments);
            return self.call(
                definitions,
                value,
                &supplied,
                environment,
                types,
                Some(&expected),
            );
        }
        if let Expression::Block(block) = &expr.value {
            let mut local = environment.clone();
            let mut types = types.clone();
            for binding in &block.bindings {
                self.local_binding(definitions, binding, &mut local, &mut types)?;
            }
            return self.check_expression(definitions, &block.value, &expected, &local, &types);
        }
        if let Expression::If {
            condition,
            then,
            otherwise,
        } = &expr.value
        {
            self.check_expression(definitions, condition, &con(BOOL), environment, types)?;
            let then = Node {
                value: Expression::Block(then.clone()),
                range: expr.range.clone(),
            };
            let result = self.check_expression(definitions, &then, &expected, environment, types);
            result?;
            let result =
                self.check_expression(definitions, otherwise, &expected, environment, types);
            result?;
            return Ok(self.substitution.apply(&expected));
        }
        if let Expression::Function(function) = &expr.value
            && function.generics.is_empty()
            && matches!(expected, Ty::Fun(_, _))
        {
            return self.function(
                definitions,
                function,
                environment,
                types,
                FunctionMode::Check(&expected),
            );
        }

        if expected == con(TYPE)
            && (matches!(expr.value, Expression::Unit)
                || matches!(&expr.value,Expression::Record(fields) if fields.is_empty()))
        {
            self.annotation(definitions, expr, types)?;
            return Ok(expected);
        }
        if expected == con(TYPE) {
            let actual = self.expression(definitions, expr, environment, types)?;
            self.unify(&actual, &con(TYPE)).map_err(|_| {
                Error("universe mismatch: expected a value of the small Type universe".into())
            })?;
            return Ok(con(TYPE));
        }
        match (&expr.value, &expected) {
            (Expression::Literal(Literal::Natural(_)), Ty::Con(INT, _)) => return Ok(expected),
            (Expression::Array(items), Ty::Con(VECTOR, args)) => {
                let args = list_values(args);
                let length = self.natural(items.len() as u128)?;
                self.unify(&length, &args[1])?;
                for item in items {
                    self.check_expression(definitions, item, &args[0], environment, types)?;
                }
                return Ok(self.substitution.apply(&expected));
            }
            (Expression::Record(fields), Ty::Con(id, args)) => {
                if let Some((names, _)) = self
                    .records
                    .iter()
                    .find(|(_, record)| record == id)
                    .cloned()
                {
                    if fields.iter().any(|(name, _)| !names.contains(name)) {
                        return Err(Error("record field set mismatch".into()));
                    }
                    let args = list_values(args);
                    if let Some(dependencies) = self.dependent_records.get(&id.0).cloned() {
                        for (offset, (field, kind)) in dependencies.iter().enumerate() {
                            let expression = fields
                                .iter()
                                .find(|(n, _)| n == &names[*field])
                                .map(|(_, e)| e)
                                .ok_or_else(|| Error("missing dependent field".into()))?;
                            let index = self.parameter_argument(
                                definitions,
                                expression,
                                types,
                                kind.clone(),
                            )?;
                            self.unify(&args[names.len() + offset], &index)?;
                        }
                    }
                    for (name, ty) in names.iter().zip(args) {
                        let found = fields
                            .iter()
                            .filter(|(field, _)| field == name)
                            .collect::<Vec<_>>();
                        if found.len() > 1 {
                            return Err(Error("duplicate record field".into()));
                        }
                        let actual_type = ty.clone();
                        if let Some(field) = found.first() {
                            self.check_expression(
                                definitions,
                                &field.1,
                                &actual_type,
                                environment,
                                types,
                            )?;
                        } else {
                            return Err(Error(
                                "record field set mismatch: missing required field".into(),
                            ));
                        }
                    }
                    return Ok(self.substitution.apply(&expected));
                }
            }
            (Expression::Block(block), _) => {
                let mut local = environment.clone();
                let mut types = types.clone();
                for binding in &block.bindings {
                    self.local_binding(definitions, binding, &mut local, &mut types)?;
                }
                return self.check_expression(definitions, &block.value, &expected, &local, &types);
            }
            _ => {}
        }
        let actual = self.expression(definitions, expr, environment, types)?;
        self.unify(&actual, &expected)?;
        Ok(self.substitution.apply(&expected))
    }
    fn block(
        &mut self,
        definitions: &Definitions<'_>,
        block: &Block,
        environment: &Environment,
        types: &Types,
    ) -> Result<Ty, Error> {
        let mut local = environment.clone();
        let mut types = types.clone();
        let mut outcome = Ok(());
        for binding in &block.bindings {
            outcome = self.local_binding(definitions, binding, &mut local, &mut types);
            if outcome.is_err() {
                break;
            }
        }
        outcome?;
        self.expression(definitions, &block.value, &local, &types)
    }
    fn expression(
        &mut self,
        definitions: &Definitions<'_>,
        expression: &Expr,
        environment: &Environment,
        types: &Types,
    ) -> Result<Ty, Error> {
        if matches!(
            &expression.value,
            Expression::Record(_) | Expression::Tuple(_)
        ) && self.syntactic_type(expression, definitions, types)
        {
            let ty = self.annotation(definitions, expression, types)?;
            return match self.universe_level(&ty) {
                0 => Ok(con(TYPE)),
                1 => Ok(con(TYPE1)),
                _ => Err(Error(
                    "universe level requires an explicit annotation".into(),
                )),
            };
        }
        match &expression.value {
            Expression::Literal(literal) => Ok(literal_type(literal)),
            Expression::Unit => Ok(con(UNIT)),
            Expression::Path(path) => self.lookup(definitions, path, environment),
            Expression::Tuple(items) => {
                let args = self.expressions(definitions, items, environment, types)?;
                Ok(Ty::Con(TUPLE, type_list(args)))
            }
            Expression::Array(items) => {
                let element = self.fresh()?;
                let mut outcome = Ok(());
                for item in items {
                    outcome = self.array_item(definitions, item, &element, environment, types);
                    if outcome.is_err() {
                        break;
                    }
                }
                outcome?;
                Ok(Ty::Con(
                    VECTOR,
                    type_list(vec![
                        self.substitution.apply(&element),
                        self.natural(items.len() as u128)?,
                    ]),
                ))
            }
            Expression::Record(fields) => {
                let sorted = sorted_fields(fields);
                let mut names = Vec::new();
                let mut args = Vec::new();
                let mut outcome = Ok(());
                for (name, index) in sorted {
                    let value = &fields[index].1;
                    if names.contains(&name) {
                        outcome = Err(Error("HM: duplicate record field".into()));
                        break;
                    }
                    names.push(name);
                    match self.expression(definitions, value, environment, types) {
                        Ok(ty) => args.push(ty),
                        Err(error) => {
                            outcome = Err(error);
                            break;
                        }
                    }
                }
                outcome?;
                Ok(Ty::Con(self.record_id(names)?, type_list(args)))
            }
            Expression::Block(block) => self.block(definitions, block, environment, types),
            Expression::Function(function) => self.function(
                definitions,
                function,
                environment,
                types,
                FunctionMode::Infer(None),
            ),
            Expression::If {
                condition,
                then,
                otherwise,
            } => {
                let condition = self.expression(definitions, condition, environment, types)?;
                self.unify(&condition, &con(BOOL))?;
                let left = self.block(definitions, then, environment, types);
                let left = left?;
                let right = self.expression(definitions, otherwise, environment, types);
                let right = right?;
                self.unify(&left, &right)?;
                Ok(self.substitution.apply(&left))
            }
            Expression::Match { value, arms } => {
                if arms.is_empty() {
                    return Err(Error("HM: empty match".into()));
                }
                let input = self.expression(definitions, value, environment, types)?;
                if self.substitution.apply(&input) == con(FACT) {
                    return Err(Error(
                        "Prop is proof irrelevant and cannot be matched".into(),
                    ));
                }
                let output = self.fresh()?;
                let mut outcome = Ok(());
                for arm in arms {
                    outcome = self.match_arm(definitions, arm, &input, &output, environment, types);
                    if outcome.is_err() {
                        break;
                    }
                }
                outcome?;
                let rows = arms
                    .iter()
                    .map(|(pattern, _)| vec![pattern.clone()])
                    .collect();
                if !self.covered(vec![self.substitution.apply(&input)], rows)? {
                    return Err(Error("non-exhaustive match".into()));
                }
                Ok(self.substitution.apply(&output))
            }
            Expression::Unary { op, value } => {
                let ty = self.expression(definitions, value, environment, types)?;
                match op {
                    UnaryOp::Not => {
                        self.unify(&ty, &con(BOOL))?;
                        Ok(con(BOOL))
                    }
                    UnaryOp::Negate => match self.substitution.apply(&ty) {
                        Ty::Con(NAT, _) | Ty::Con(INT, _) => Ok(con(INT)),
                        _ => Err(Error(
                            "negation requires an integer or fixed-width bits".into(),
                        )),
                    },
                }
            }
            Expression::Binary {
                op: BinaryOp::Arrow,
                ..
            } => {
                let ty = self.annotation(definitions, expression, types)?;
                Ok(if self.universe_level(&ty) == 0 {
                    con(TYPE)
                } else {
                    con(TYPE1)
                })
            }
            Expression::Binary { op, left, right } => {
                self.binary(definitions, *op, left, right, environment, types)
            }
            Expression::Call { value, arguments } => {
                self.call(definitions, value, arguments, environment, types, None)
            }
            Expression::Member { value, name } => {
                if let Some(kind) =
                    self.operation_member(definitions, value, name, environment, types)?
                {
                    return Ok(kind);
                }
                let ty = self.expression(definitions, value, environment, types)?;
                let mut found = None;
                if let Ty::Con(id, args) = self.substitution.apply(&ty) {
                    if id == TUPLE {
                        return name
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| list_values(&args).get(index).cloned())
                            .ok_or_else(|| Error("tuple projection is outside its fields".into()));
                    }
                    for (fields, record) in &self.records {
                        if *record == id {
                            found = record_member(fields, name, &args);
                            break;
                        }
                    }
                }
                if let Some(ty) = found {
                    return Ok(ty);
                }
                Err(Error(format!(
                    "HM: field {name} requires a known closed record"
                )))
            }
            Expression::Generic { value, arguments } => {
                self.type_application(definitions, value, arguments, environment, types)
            }
            Expression::RecordCall {
                value,
                fields,
                arguments,
            } => {
                let record = Node {
                    value: Expression::Record(fields.clone()),
                    range: expression.range.clone(),
                };
                let mut supplied = vec![record];
                supplied.extend_from_slice(arguments);
                self.call(definitions, value, &supplied, environment, types, None)
            }
            Expression::Op(fields) => {
                self.operation(definitions, expression, fields, None, environment, types)
            }
            Expression::Ir { bindings, value } => self.block(
                definitions,
                &Block {
                    bindings: bindings.clone(),
                    value: value.clone(),
                },
                environment,
                types,
            ),
            Expression::Limit(fields) => self.expression(
                definitions,
                &Node {
                    value: Expression::Record(fields.clone()),
                    range: expression.range.clone(),
                },
                environment,
                types,
            ),
        }
    }
    fn axiom(
        &mut self,
        definitions: &Definitions<'_>,
        path: &Path,
        generics: &[RuleBinder],
        annotation: &Expr,
    ) -> Result<(), Error> {
        if self.environment.contains_key(path) {
            return Ok(());
        }
        if !self.checking_axioms.insert(path.clone()) {
            return Err(Error("cyclic declaration type".into()));
        }
        let mut local = Environment::new();
        let mut types = self.types.clone();
        let mut parameters = Vec::new();
        for binder in generics {
            if local.contains_key(&vec![binder.name.clone()]) {
                return Err(Error("duplicate declaration binder".into()));
            }
            let ty = match &binder.ty {
                Some(ty) => self.annotation(definitions, ty, &types)?,
                None => self.fresh()?,
            };
            let index = self.fresh()?;
            if let Ty::Var(id) = &index {
                let kind = ty.clone();
                self.index_types.insert(id.0, kind);
            }
            parameters.push(index.clone());
            types.insert(vec![binder.name.clone()], Some(index));
            local.insert(vec![binder.name.clone()], BoundValue::mono(ty));
        }
        let relation = match &annotation.value {
            Expression::Binary {
                op: BinaryOp::Arrow,
                right,
                ..
            } => matches!(&right.value, Expression::Path(name) if name == &vec!["Prop".to_owned()]),
            _ => false,
        };
        let ty = if relation {
            let ty = self.annotation(definitions, annotation, &types)?;
            self.relations.insert(path.clone());
            ty
        } else if clause_annotation(annotation) {
            let (premises, heads) = match &annotation.value {
                Expression::Binary {
                    op: BinaryOp::Arrow,
                    left,
                    right,
                } => (Some(&**left), &**right),
                _ => (None, annotation),
            };
            let mut matches = MatchArguments::new();
            if let Some(premises) = premises {
                self.propositions(definitions, premises, &local, &types, Some(&mut matches))?;
            }
            self.propositions(definitions, heads, &local, &types, None)?;
            if !matches.is_empty() {
                let matches = self.checked_matches(matches)?;
                self.operation_matches.insert(path.clone(), matches);
            }
            let binding = if let Some(premises) = premises {
                Some(premises)
            } else if let Expression::Binary {
                op: BinaryOp::Equal,
                left,
                ..
            } = &heads.value
            {
                Some(&**left)
            } else {
                None
            };
            let bound = binding
                .map(crate::totality::free_paths)
                .transpose()?
                .unwrap_or_default();
            if generics
                .iter()
                .any(|binder| !bound.contains(&vec![binder.name.clone()]))
            {
                return Err(Error(
                    "every clause binder must occur in a premise or the left side of an equation"
                        .into(),
                ));
            }
            con(FACT)
        } else {
            self.annotation(definitions, annotation, &types)?
        };
        let ty = self.substitution.apply(&ty);
        if !relation && !clause_annotation(annotation) {
            let (mut domains, result) = match &ty {
                Ty::Fun(parameters, result) => (list_values(parameters), (**result).clone()),
                _ => (Vec::new(), ty.clone()),
            };
            if type_family(annotation) {
                domains = generics
                    .iter()
                    .map(|binder| local[&vec![binder.name.clone()]].ty.clone())
                    .collect();
            }
            let id = if let Some(Some(Ty::Con(id, _))) = self.types.get(path) {
                *id
            } else {
                self.constructor()?
            };
            self.types
                .insert(path.clone(), Some(Ty::Con(id, type_list(domains.clone()))));
            self.arities.insert(path.clone(), domains.len());
            self.declared.insert(path.clone(), (id, domains, result));
        }
        let scheme = self.generalize(&self.environment, &ty);
        if !parameters.is_empty() && !clause_annotation(annotation) {
            self.explicit.insert(path.clone(), parameters);
        }
        self.environment.insert(path.clone(), scheme.into());
        self.checking_axioms.remove(path);
        Ok(())
    }
    fn propositions(
        &mut self,
        definitions: &Definitions<'_>,
        expression: &Expr,
        local: &Environment,
        types: &Types,
        mut matches: Option<&mut MatchArguments>,
    ) -> Result<(), Error> {
        match &expression.value {
            Expression::Tuple(items) => {
                for item in items {
                    self.propositions(definitions, item, local, types, matches.as_deref_mut())?;
                }
            }
            Expression::Unary {
                op: UnaryOp::Not,
                value,
            } if matches.is_some() => {
                let ty = self.proposition_term(definitions, value, local, types, None)?;
                self.unify(&ty, &con(FACT))?;
            }
            Expression::Binary {
                op: BinaryOp::Equal,
                left,
                right,
            } => {
                let left = self.proposition_term(definitions, left, local, types, None)?;
                let right = self.proposition_term(definitions, right, local, types, None)?;
                self.unify(&left, &right)?;
            }
            _ => {
                let ty = self.proposition_term(definitions, expression, local, types, matches)?;
                self.unify(&ty, &con(FACT))?;
            }
        }
        Ok(())
    }
    fn proposition_term(
        &mut self,
        definitions: &Definitions<'_>,
        expression: &Expr,
        local: &Environment,
        types: &Types,
        matches: Option<&mut MatchArguments>,
    ) -> Result<Ty, Error> {
        if let Expression::Call { value, arguments } = &expression.value {
            let callee = self.expression(definitions, value, local, types)?;
            let parameters = self.fresh_types(arguments.len())?;
            let result = self.fresh()?;
            self.unify(
                &callee,
                &Ty::Fun(type_list(parameters.clone()), Box::new(result.clone())),
            )?;
            if parameters.len() != arguments.len() {
                return Err(Error("clause application arity mismatch".into()));
            }
            for (argument, parameter) in arguments.iter().zip(&parameters) {
                if matches!(argument.value, Expression::Literal(_)) {
                    self.check_expression(definitions, argument, parameter, local, types)?;
                } else {
                    let ty = self.proposition_term(definitions, argument, local, types, None)?;
                    self.unify(&ty, parameter)?;
                }
            }
            if let Some(matches) = matches {
                let groups = match (&value.value, parameters.as_slice()) {
                    (Expression::Member { name, .. }, [_, operands, results])
                        if name == "matches" =>
                    {
                        Some([operands, results])
                    }
                    (Expression::Path(path), [_, _, operands, results])
                        if path == &["std", "ir", "match_kind"] =>
                    {
                        Some([operands, results])
                    }
                    _ => None,
                };
                if let Some(groups) = groups {
                    matches.insert(expression.range.start, groups.map(Ty::clone));
                }
            }
            return Ok(self.substitution.apply(&result));
        }
        self.expression(definitions, expression, local, types)
    }
    fn type_application(
        &mut self,
        definitions: &Definitions<'_>,
        value: &Expr,
        arguments: &[Expr],
        environment: &Environment,
        types: &Types,
    ) -> Result<Ty, Error> {
        let Expression::Path(path) = &value.value else {
            return Err(Error(
                "HM: type application requires a named generic value".into(),
            ));
        };
        if environment.contains_key(path) {
            return Err(Error(
                "HM: explicit type application of local values is unsupported".into(),
            ));
        }
        if (!self.explicit.contains_key(path)
            || self.axioms.get(path).is_some_and(|(_, ty)| type_family(ty)))
            && (self.arities.contains_key(path) || self.declared.contains_key(path))
        {
            let ty = self.constructor_annotation(definitions, path, arguments, types)?;
            if let Some((_, _, result)) = self.declared.get(path) {
                return Ok(result.clone());
            }
            return Ok(if self.universe_level(&ty) == 0 {
                con(TYPE)
            } else {
                con(TYPE1)
            });
        }
        self.definition(definitions, path)?;
        if self.roots.contains_key(path) {
            return Err(Error("HM: polymorphic recursion is unsupported".into()));
        }
        let parameters = self
            .explicit
            .get(path)
            .cloned()
            .ok_or_else(|| Error("HM: value has no explicit universal parameters".into()))?;
        if parameters.len() != arguments.len() {
            return Err(Error("HM: explicit type argument arity mismatch".into()));
        }
        let scheme = self
            .environment
            .get(path)
            .cloned()
            .ok_or_else(|| Error("HM: missing generic value".into()))?;
        let scheme = self.substitution.apply_scheme(&scheme);
        let mut substitution = self.type_arguments(definitions, &parameters, arguments, types)?;
        let mut outcome = Ok(());
        for id in &scheme.quantified {
            let mut supplied = false;
            for (parameter, _) in &substitution.bindings {
                if parameter == id {
                    supplied = true;
                    break;
                }
            }
            if !supplied {
                match self.fresh() {
                    Ok(ty) => substitution.bindings.push((*id, ty)),
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }
        }
        outcome?;
        Ok(substitution.apply(&scheme.ty))
    }
    fn type_arguments(
        &mut self,
        definitions: &Definitions<'_>,
        parameters: &[Ty],
        arguments: &[Expr],
        types: &Types,
    ) -> Result<Substitution, Error> {
        let mut substitution = Substitution::default();
        let mut index = 0;
        loop {
            if index == parameters.len() {
                break Ok(substitution);
            }
            match self.type_argument(
                definitions,
                &parameters[index],
                &arguments[index],
                types,
                &substitution,
            ) {
                Ok(value) => {
                    substitution.bindings.push(value);
                    index += 1;
                }
                Err(error) => break Err(error),
            }
        }
    }
    fn function(
        &mut self,
        definitions: &Definitions<'_>,
        function: &Function,
        environment: &Environment,
        types: &Types,
        mode: FunctionMode<'_>,
    ) -> Result<Ty, Error> {
        let (owner, expected_signature) = match mode {
            FunctionMode::Infer(owner) => (owner, None),
            FunctionMode::Check(ty) => (None, Some(ty)),
        };

        if function.decreases.is_some() && owner.is_none() {
            return Err(Error(
                "decreases requires a named recursive function".into(),
            ));
        }
        let (mut types, ids) = self.generics(definitions, &function.generics, types)?;
        let local_rigid_start = self.rigid;
        let mut local = environment.clone();
        for (binder, id) in function.generics.iter().zip(&ids) {
            let kind = self
                .rigid_types
                .get(&id.0)
                .ok_or_else(|| Error("generic type was not elaborated".into()))?
                .clone();
            local.insert(vec![binder.name.clone()], BoundValue::mono(kind));
        }
        let mut args = Vec::new();
        let mut names = Vec::new();
        let mut outcome = Ok(());
        for binder in &function.parameters {
            if names.contains(&binder.name) {
                outcome = Err(Error("HM: duplicate parameter".into()));
                break;
            }
            names.push(binder.name.clone());
            match self.annotation(definitions, &binder.ty, &types) {
                Ok(ty) => {
                    args.push(ty.clone());
                    let parameter = BoundValue::mono(ty.clone());
                    local.insert(vec![binder.name.clone()], parameter);
                    let kind = Some(ty.clone());
                    if let Some(kind) = kind {
                        let id = RigidId(self.rigid);
                        self.rigid = self
                            .rigid
                            .checked_add(1)
                            .ok_or_else(|| Error("rigid identities exhausted".into()))?;
                        self.rigid_types.insert(id.0, kind);
                        types.insert(vec![binder.name.clone()], Some(Ty::Rigid(id)));
                    } else {
                        types.insert(vec![binder.name.clone()], None);
                    }
                }
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        outcome?;
        let contextual_result = if let Some(Ty::Fun(parameters, result)) = expected_signature {
            self.unify(
                &Ty::Con(TUPLE, type_list(args.clone())),
                &Ty::Con(TUPLE, parameters.clone()),
            )?;
            Some((**result).clone())
        } else {
            None
        };
        let expected = match &function.result {
            Some(annotation) => self.annotation(definitions, annotation, &types)?,
            None => {
                if let Some(result) = &contextual_result {
                    result.clone()
                } else {
                    self.fresh()?
                }
            }
        };
        if let Some(result) = contextual_result {
            self.unify(&expected, &result)?;
        }
        if let Some(path) = owner {
            let recursive = self
                .environment
                .get(path)
                .cloned()
                .ok_or_else(|| Error("HM: missing recursive signature".into()))?;
            self.unify(
                &recursive.ty,
                &Ty::Fun(type_list(args.clone()), Box::new(expected.clone())),
            )?;
        }
        let mut body_local = local.clone();
        let mut body_types = types.clone();
        for binding in &function.body.bindings {
            self.local_binding(definitions, binding, &mut body_local, &mut body_types)?;
        }
        let result = self.check_expression(
            definitions,
            &function.body.value,
            &expected,
            &body_local,
            &body_types,
        )?;
        self.unify(&result, &expected)?;
        let signature = Ty::Fun(type_list(args), Box::new(result));
        let local_rigids = (local_rigid_start..self.rigid)
            .map(RigidId)
            .collect::<Vec<_>>();
        if contains_rigid(&signature, &local_rigids) {
            return Err(Error("dependent function signatures require explicit equality/elimination evidence; use quantified generic parameters for rank-one polymorphism".into()));
        }
        self.discharge(&signature, &ids, environment, owner)
    }
}

fn literal_type(literal: &Literal) -> Ty {
    con(match literal {
        Literal::Natural(_) => NAT,
        Literal::String(_) => STRING,
        Literal::Bool(_) => BOOL,
    })
}
fn kernel_error(error: hm::UnifyError) -> Error {
    Error(format!("HM: {error:?}"))
}
fn record_member(fields: &[String], name: &str, args: &TyList) -> Option<Ty> {
    let mut index = 0;
    loop {
        if index == fields.len() {
            break None;
        }
        if fields[index] == name {
            break Some(list_values(args)[index].clone());
        }
        index += 1;
    }
}

fn sorted_fields(fields: &[(String, Expr)]) -> Vec<(String, usize)> {
    let mut sorted = Vec::new();
    for (index, (name, _)) in fields.iter().enumerate() {
        sorted.push((name.clone(), index));
    }
    sorted.sort();
    sorted
}
fn contains_rigid(ty: &Ty, ids: &[RigidId]) -> bool {
    match ty {
        Ty::Rigid(id) => ids.contains(id),
        Ty::Var(_) => false,
        Ty::Con(_, args) => contains_rigid_list(args, ids),
        Ty::Fun(args, result) => contains_rigid_list(args, ids) || contains_rigid(result, ids),
    }
}
fn contains_rigid_list(types: &TyList, ids: &[RigidId]) -> bool {
    let mut current = types;
    let mut found = false;
    while let TyList::Cons(head, tail) = current {
        if contains_rigid(head, ids) {
            found = true;
            break;
        }
        current = tail;
    }
    found
}
fn replace_rigid(ty: &Ty, replacements: &[(RigidId, Ty)]) -> Ty {
    match ty {
        Ty::Rigid(id) => {
            let mut index = 0;
            loop {
                if index == replacements.len() {
                    break ty.clone();
                }
                let (key, value) = &replacements[index];
                if key == id {
                    break value.clone();
                }
                index += 1;
            }
        }
        Ty::Var(_) => ty.clone(),
        Ty::Con(id, args) => Ty::Con(*id, replace_rigid_list(args, replacements)),
        Ty::Fun(args, result) => Ty::Fun(
            replace_rigid_list(args, replacements),
            Box::new(replace_rigid(result, replacements)),
        ),
    }
}
fn replace_rigid_list(types: &TyList, replacements: &[(RigidId, Ty)]) -> Box<TyList> {
    let mut result = Vec::new();
    let mut current = types;
    while let TyList::Cons(head, tail) = current {
        result.push(replace_rigid(head, replacements));
        current = tail;
    }
    type_list(result)
}
fn type_list(mut types: Vec<Ty>) -> Box<TyList> {
    let mut result = Box::new(TyList::Nil);
    while let Some(ty) = types.pop() {
        result = Box::new(TyList::Cons(Box::new(ty), result));
    }
    result
}
fn list_values(types: &TyList) -> Vec<Ty> {
    let mut result = Vec::new();
    let mut current = types;
    while let TyList::Cons(head, tail) = current {
        result.push((**head).clone());
        current = tail;
    }
    result
}

fn declaration_location(
    module: &crate::compiler::CompiledModule,
    declaration: &Node<Declaration>,
) -> String {
    let offset = match module.ast.token_spans.get(declaration.range.start) {
        Some(crate::lexer::Span::Source(range)) => range.start,
        _ => 0,
    };
    format!("{}:{offset}", module.file.display())
}

fn declaration_error(
    module: &crate::compiler::CompiledModule,
    declaration: &Node<Declaration>,
    error: Error,
) -> Error {
    let location = declaration_location(module, declaration);
    Error(format!("{location}: {error}"))
}

fn contains_constructor(ty: &Ty, family: ConId) -> bool {
    match ty {
        Ty::Con(id, args) => {
            *id == family
                || list_values(args)
                    .iter()
                    .any(|ty| contains_constructor(ty, family))
        }
        Ty::Fun(args, result) => {
            list_values(args)
                .iter()
                .any(|ty| contains_constructor(ty, family))
                || contains_constructor(result, family)
        }
        _ => false,
    }
}
fn check_positive(ty: &Ty, family: ConId) -> Result<(), Error> {
    match ty {
        Ty::Fun(args, result) => {
            if list_values(args)
                .iter()
                .any(|ty| contains_constructor(ty, family))
            {
                return Err(Error(
                    "inductive occurrence is not strictly positive".into(),
                ));
            }
            check_positive(result, family)
        }
        Ty::Con(id, args) => {
            if list_values(args)
                .iter()
                .any(|ty| contains_constructor(ty, family))
            {
                return Err(Error(format!(
                    "nested inductive occurrence through {id:?} requires a positivity certificate"
                )));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn catch_all(pattern: &Pattern) -> bool {
    matches!(&pattern.value, PatternKind::Wildcard)
        || matches!(&pattern.value,PatternKind::Path(path) if path.len()==1)
}

fn any_rigid(ty: &Ty) -> bool {
    match ty {
        Ty::Rigid(_) => true,
        Ty::Con(_, args) => list_values(args).iter().any(any_rigid),
        Ty::Fun(args, result) => list_values(args).iter().any(any_rigid) || any_rigid(result),
        Ty::Var(_) => false,
    }
}

fn clause_annotation(annotation: &Expr) -> bool {
    match &annotation.value {
        Expression::Binary {
            op: BinaryOp::Equal,
            ..
        }
        | Expression::Call { .. }
        | Expression::RecordCall { .. }
        | Expression::Unary {
            op: UnaryOp::Not, ..
        } => true,
        Expression::Tuple(items) => items.iter().any(clause_annotation),
        Expression::Binary {
            op: BinaryOp::Arrow,
            left,
            ..
        } => clause_annotation(left),
        _ => false,
    }
}

fn type_family(annotation: &Expr) -> bool {
    matches!(&annotation.value, Expression::Path(path) if path == &["Type"])
}
