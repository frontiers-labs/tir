use crate::ast::{
    self, Expr, Expression, Generic, GenericArg, ItemKind, Literal, Path, PatternSyntax, Span,
    TypeExpr, TypeSyntax,
};
use crate::core::{Definition, NominalDefinition, Pattern, Scheme, Term, TermKind};
use crate::diagnostic::{Diagnostic, Phase};
use crate::eval::{Evaluator, Value};
use crate::resolve::{Entry, Symbols, qualify};
use crate::types::{Constraint, Type};
use std::collections::{BTreeMap, BTreeSet};

type Checked<T> = Result<T, Diagnostic>;
type Environment = BTreeMap<String, Type>;
#[derive(Clone)]
struct Obligation {
    constraint: Constraint,
    span: Span,
}
#[derive(Clone)]
struct EnumInfo {
    parameters: Vec<(String, Type)>,
    variants: Vec<(String, Vec<Type>)>,
}

pub(crate) struct Checker {
    symbols: Symbols,
    pub definitions: BTreeMap<String, Definition>,
    pub nominals: BTreeMap<String, NominalDefinition>,
    substitutions: BTreeMap<u32, Type>,
    next_variable: u32,
    visiting: BTreeSet<String>,
    active_definitions: Vec<String>,
    recursive_schemes: BTreeMap<String, Scheme>,
    aliases: BTreeSet<String>,
    enums: BTreeMap<String, EnumInfo>,
    constructors: BTreeMap<String, Scheme>,
    obligations: Vec<Obligation>,
    package: String,
    source: String,
    module: String,
    parameters: Environment,
    parameter_kinds: Environment,
    variable_kinds: BTreeMap<u32, Type>,
    nominal_levels: BTreeMap<String, u32>,
    defer_completion: bool,
}
impl Checker {
    pub fn new(symbols: Symbols, package: &str) -> Self {
        Self {
            symbols,
            definitions: BTreeMap::new(),
            nominals: BTreeMap::new(),
            substitutions: BTreeMap::new(),
            next_variable: 0,
            visiting: BTreeSet::new(),
            active_definitions: Vec::new(),
            recursive_schemes: BTreeMap::new(),
            aliases: BTreeSet::new(),
            enums: BTreeMap::new(),
            constructors: BTreeMap::new(),
            obligations: Vec::new(),
            package: package.into(),
            source: String::new(),
            module: String::new(),
            parameters: BTreeMap::new(),
            parameter_kinds: BTreeMap::new(),
            variable_kinds: BTreeMap::new(),
            nominal_levels: BTreeMap::new(),
            defer_completion: false,
        }
    }
    pub fn infer_definitions(self) -> Result<Self, Vec<Diagnostic>> {
        Self {
            defer_completion: true,
            ..self
        }
        .check()
    }
    pub fn check(mut self) -> Result<Self, Vec<Diagnostic>> {
        let entries = self.symbols.entries.clone();
        for (name, entry) in &entries {
            if let ItemKind::ExternalType(_, generics, category) = &entry.kind {
                let category = self
                    .with_entry(entry, |this| {
                        this.generic_parameters(generics)?;
                        this.type_expr(category)
                    })
                    .map_err(|e| vec![e])?;
                self.nominal_levels.insert(
                    name.clone(),
                    if let Type::Universe(level) = category {
                        level
                    } else {
                        0
                    },
                );
            }
        }
        for (name, entry) in &entries {
            if matches!(entry.kind, ItemKind::Enum(..)) {
                self.prepare_enum(name, entry).map_err(|e| vec![e])?;
            }
        }
        self.check_mutual_inductives().map_err(|e| vec![e])?;
        for (name, entry) in entries {
            self.source = entry.source.clone();
            self.module = entry.module.clone();
            self.parameters.clear();
            self.parameter_kinds.clear();
            match &entry.kind {
                ItemKind::Let { .. } | ItemKind::ExternalValue(..) => {
                    self.definition(&name).map_err(|e| vec![e])?;
                }
                ItemKind::Type(_, generics, body) => {
                    self.parameters = self
                        .generic_parameters(generics)
                        .map_err(|e| vec![e])?
                        .into_iter()
                        .map(|(n, k)| {
                            (
                                n.clone(),
                                if matches!(k, Type::Universe(_)) {
                                    Type::Parameter(n)
                                } else {
                                    Type::Index(n, Box::new(k))
                                },
                            )
                        })
                        .collect();
                    let start = self.obligations.len();
                    self.type_expr(body).map_err(|e| vec![e])?;
                    self.finish_constraints(start, !generics.is_empty())
                        .map_err(|e| vec![e])?;
                }
                ItemKind::ExternalType(_, generics, category) => {
                    let parameters = self.generic_parameters(generics).map_err(|e| vec![e])?;
                    let category = self.type_expr(category).map_err(|e| vec![e])?;
                    if !matches!(category, Type::Universe(_) | Type::Nominal(..)) {
                        return Err(vec![self.error(entry.span.clone(),"an external type category must be a universe or an explicitly declared nominal category")]);
                    }
                    self.nominals.insert(
                        name,
                        NominalDefinition {
                            parameters,
                            category,
                        },
                    );
                }
                _ => {}
            }
        }
        Ok(self)
    }
    pub fn complete(mut self, requirements: &[(String, Type)]) -> Result<Self, Vec<Diagnostic>> {
        for (name, expected) in requirements {
            let definition = self.definitions.get(name).ok_or_else(|| {
                vec![self.error(0..0, format!("unknown checked binding `{name}`"))]
            })?;
            self.source = definition.source.clone();
            self.parameter_kinds = definition.scheme.parameters.iter().cloned().collect();
            self.validate_requirement(
                expected,
                &definition.scheme.ty.variables(),
                &definition.term.span,
            )
            .map_err(|error| vec![error])?;
        }
        for (name, expected) in requirements {
            let definition = self.definitions.get(name).cloned().ok_or_else(|| {
                vec![self.error(0..0, format!("unknown checked binding `{name}`"))]
            })?;
            self.source = definition.source;
            self.parameter_kinds = definition.scheme.parameters.iter().cloned().collect();
            self.unify(&definition.scheme.ty, expected, &definition.term.span)
                .map_err(|error| vec![error])?;
        }
        let mut definitions = std::mem::take(&mut self.definitions);
        for definition in definitions.values_mut() {
            self.source = definition.source.clone();
            let start = self.obligations.len();
            self.obligations
                .extend(
                    definition
                        .scheme
                        .constraints
                        .iter()
                        .map(|constraint| Obligation {
                            constraint: constraint.clone(),
                            span: definition.term.span.clone(),
                        }),
                );
            definition.scheme.constraints = self
                .finish_constraints(start, !definition.scheme.parameters.is_empty())
                .map_err(|error| vec![error])?;
        }
        for definition in definitions.values_mut() {
            definition.scheme.ty = self.resolve_type(&definition.scheme.ty);
            for (_, kind) in &mut definition.scheme.parameters {
                *kind = self.resolve_type(kind);
            }
            definition.term.map_types(&mut |ty| self.resolve_type(ty));
            self.source = definition.source.clone();
            self.reject_ambiguity(&definition.scheme.ty, &definition.term)
                .map_err(|error| vec![error])?;
        }
        self.definitions = definitions;
        self.defer_completion = false;
        Ok(self)
    }
    fn validate_requirement(&self, ty: &Type, rows: &BTreeSet<u32>, span: &Span) -> Checked<()> {
        match ty {
            Type::Parameter(name) => {
                if !matches!(self.parameter_kinds.get(name), Some(Type::Universe(_))) {
                    return Err(
                        self.error(span.clone(), format!("unknown type parameter `{name}`"))
                    );
                }
            }
            Type::Nominal(name, arguments) => {
                let parameters = if name == "Option" {
                    vec![("T".into(), Type::Universe(u32::MAX))]
                } else if let Some(nominal) = self.nominals.get(name) {
                    nominal.parameters.clone()
                } else if let Some(info) = self.enums.get(name) {
                    info.parameters.clone()
                } else {
                    return Err(self.error(span.clone(), format!("unknown nominal type `{name}`")));
                };
                if arguments.len() != parameters.len() {
                    return Err(self.error(
                        span.clone(),
                        format!(
                            "`{name}` expects {} generic arguments, found {}",
                            parameters.len(),
                            arguments.len()
                        ),
                    ));
                }
                for (argument, (_, kind)) in arguments.iter().zip(&parameters) {
                    if matches!(kind, Type::Universe(_)) {
                        self.validate_requirement(argument, rows, span)?;
                    } else if let Type::Index(name, declared) = argument
                        && self.parameter_kinds.get(name) != Some(declared.as_ref())
                    {
                        return Err(self.error(
                            span.clone(),
                            format!("unknown or mismatched static parameter `{name}`"),
                        ));
                    }
                    self.check_argument_kind(argument, kind, span)?;
                }
            }
            Type::Function(arguments, result) => {
                let mut names = BTreeSet::new();
                for (name, argument) in arguments {
                    if !names.insert(name) {
                        return Err(
                            self.error(span.clone(), format!("duplicate parameter `{name}`"))
                        );
                    }
                    self.validate_requirement(argument, rows, span)?;
                }
                self.validate_requirement(result, rows, span)?;
            }
            Type::Tuple(fields) => {
                let mut names = BTreeSet::new();
                for (name, field) in fields {
                    if let Some(name) = name
                        && !names.insert(name)
                    {
                        return Err(
                            self.error(span.clone(), format!("duplicate product field `{name}`"))
                        );
                    }
                    self.validate_requirement(field, rows, span)?;
                }
            }
            Type::Record(fields, tail) => {
                for field in fields.values() {
                    self.validate_requirement(field, rows, span)?;
                }
                if let Some(tail) = tail {
                    match tail.as_ref() {
                        Type::Variable(variable) if rows.contains(variable) => {}
                        _ => {
                            return Err(self.error(
                                span.clone(),
                                "record tail must be an inferred row of this binding",
                            ));
                        }
                    }
                }
            }
            Type::Description(ty) | Type::List(ty) | Type::Named(ty) => {
                self.validate_requirement(ty, rows, span)?;
            }
            Type::Variable(variable) => {
                return Err(self.error(
                    span.clone(),
                    format!("inference variable ?{variable} is not a checked type requirement"),
                ));
            }
            Type::Index(..)
            | Type::IntegerIndex(_)
            | Type::BooleanIndex(_)
            | Type::StringIndex(_) => {
                return Err(self.error(
                    span.clone(),
                    format!("static value {ty} cannot be used as a type"),
                ));
            }
            Type::Universe(level) if level.checked_add(1).is_none() => {
                return Err(self.error(span.clone(), "type universe level overflows"));
            }
            Type::Universe(_) | Type::Bool | Type::Integer | Type::Nat | Type::String => {}
        }
        Ok(())
    }
    fn namespace(&self, entry: &Entry) -> Option<String> {
        let path = match &entry.kind {
            ItemKind::Let { path, .. } | ItemKind::ExternalValue(path, _, _) => path,
            _ => return None,
        };
        (path.segments.len() > 1)
            .then(|| {
                self.symbols.resolve(
                    &entry.module,
                    &Path {
                        absolute: false,
                        segments: path.segments[..path.segments.len() - 1].to_vec(),
                    },
                )
            })
            .flatten()
    }
    fn check_mutual_inductives(&self) -> Checked<()> {
        fn reaches(
            enums: &BTreeMap<String, EnumInfo>,
            current: &str,
            target: &str,
            seen: &mut BTreeSet<String>,
        ) -> bool {
            if !seen.insert(current.into()) {
                return false;
            }
            let Some(info) = enums.get(current) else {
                return false;
            };
            info.variants
                .iter()
                .flat_map(|(_, fields)| fields)
                .any(|field| {
                    let mut dependencies = BTreeSet::new();
                    field.map(&mut |ty| {
                        if let Type::Nominal(n, _) = ty
                            && n != current
                        {
                            dependencies.insert(n.clone());
                        }
                        None
                    });
                    dependencies
                        .iter()
                        .any(|n| n == target || reaches(enums, n, target, seen))
                })
        }
        for (name, info) in &self.enums {
            for (target, target_info) in &self.enums {
                if name != target
                    && reaches(&self.enums, name, target, &mut BTreeSet::new())
                    && reaches(&self.enums, target, name, &mut BTreeSet::new())
                {
                    let parameters = target_info
                        .parameters
                        .iter()
                        .map(|(name, kind)| {
                            if matches!(kind, Type::Universe(_)) {
                                Type::Parameter(name.clone())
                            } else {
                                Type::Index(name.clone(), Box::new(kind.clone()))
                            }
                        })
                        .collect::<Vec<_>>();
                    for (_, fields) in &info.variants {
                        for field in fields {
                            self.check_positive(target, &parameters, field, true, &(0..0))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
    pub(crate) fn inductive_constructors(&self, name: &str) -> Option<&[(String, Vec<Type>)]> {
        self.enums.get(name).map(|info| info.variants.as_slice())
    }
    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(&self.source, span, Phase::Type, message)
    }
    fn fresh(&mut self) -> Type {
        let n = self.next_variable;
        self.next_variable += 1;
        Type::Variable(n)
    }
    fn resolve_type(&self, ty: &Type) -> Type {
        match ty {
            Type::Variable(v) => self
                .substitutions
                .get(v)
                .map_or_else(|| ty.clone(), |t| self.resolve_type(t)),
            _ => ty.map(&mut |t| {
                if t != ty {
                    Some(self.resolve_type(t))
                } else {
                    None
                }
            }),
        }
    }
    fn bind(&mut self, variable: u32, ty: &Type, span: &Span) -> Checked<()> {
        if ty == &Type::Variable(variable) {
            return Ok(());
        }
        if ty.variables().contains(&variable) {
            return Err(self.error(span.clone(), "infinite type rejected by occurs check"));
        }
        if let Some(kind) = self.variable_kinds.get(&variable).cloned() {
            if let Type::Variable(other) = ty {
                let kind = match (kind, self.variable_kinds.get(other)) {
                    (Type::Universe(a), Some(Type::Universe(b))) => Type::Universe(a.min(*b)),
                    (a, Some(b)) if a != *b => {
                        return Err(self.error(
                            span.clone(),
                            format!("incompatible generic kinds {a} and {b}"),
                        ));
                    }
                    (kind, _) => kind,
                };
                self.variable_kinds.insert(*other, kind);
            } else {
                self.check_argument_kind(ty, &kind, span)?;
            }
        }
        self.substitutions.insert(variable, ty.clone());
        Ok(())
    }
    fn unify(&mut self, left: &Type, right: &Type, span: &Span) -> Checked<()> {
        let left = self.resolve_type(left);
        let right = self.resolve_type(right);
        if left == right {
            return Ok(());
        }
        match (&left, &right) {
            (Type::Variable(v), t) | (t, Type::Variable(v)) => self.bind(*v, t, span),
            (Type::Function(a, r), Type::Function(b, s)) if a.len() == b.len() => {
                for ((_, a), (_, b)) in a.iter().zip(b) {
                    self.unify(a, b, span)?;
                }
                self.unify(r, s, span)
            }
            (Type::Tuple(a), Type::Tuple(b))
                if a.len() == b.len() && a.iter().zip(b).all(|((n, _), (m, _))| n == m) =>
            {
                for ((_, a), (_, b)) in a.iter().zip(b) {
                    self.unify(a, b, span)?;
                }
                Ok(())
            }
            (Type::Record(a, at), Type::Record(b, bt)) => {
                self.unify_rows(a, at.as_deref(), b, bt.as_deref(), span)
            }
            (Type::Tuple(fields), Type::Record(row, tail))
            | (Type::Record(row, tail), Type::Tuple(fields))
                if !fields.is_empty() && fields.iter().all(|(n, _)| n.is_some()) =>
            {
                let fields = fields
                    .iter()
                    .filter_map(|(n, t)| n.as_ref().map(|n| (n.clone(), t.clone())))
                    .collect();
                self.unify_rows(&fields, None, row, tail.as_deref(), span)
            }
            (Type::Nominal(a, aa), Type::Nominal(b, bb)) if a == b && aa.len() == bb.len() => {
                for (a, b) in aa.iter().zip(bb) {
                    self.unify(a, b, span)?;
                }
                Ok(())
            }
            (Type::Description(a), Type::Description(b))
            | (Type::List(a), Type::List(b))
            | (Type::Named(a), Type::Named(b)) => self.unify(a, b, span),
            _ => Err(self.error(
                span.clone(),
                format!("type mismatch: expected {right}, found {left}"),
            )),
        }
    }
    fn unify_rows(
        &mut self,
        a: &BTreeMap<String, Type>,
        at: Option<&Type>,
        b: &BTreeMap<String, Type>,
        bt: Option<&Type>,
        span: &Span,
    ) -> Checked<()> {
        for (n, t) in a {
            if let Some(u) = b.get(n) {
                self.unify(t, u, span)?;
            }
        }
        let a_only: BTreeMap<_, _> = a
            .iter()
            .filter(|(n, _)| !b.contains_key(*n))
            .map(|(n, t)| (n.clone(), t.clone()))
            .collect();
        let b_only: BTreeMap<_, _> = b
            .iter()
            .filter(|(n, _)| !a.contains_key(*n))
            .map(|(n, t)| (n.clone(), t.clone()))
            .collect();
        if !a_only.is_empty() && bt.is_none() || !b_only.is_empty() && at.is_none() {
            return Err(self.error(
                span.clone(),
                format!(
                    "record fields do not match: {{{}}} and {{{}}}",
                    a.keys().cloned().collect::<Vec<_>>().join(", "),
                    b.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        match (at, bt) {
            (Some(at), Some(bt)) => {
                if a_only.is_empty() && b_only.is_empty() {
                    self.unify(at, bt, span)
                } else {
                    let tail = self.fresh();
                    self.unify(
                        at,
                        &Type::Record(b_only, Some(Box::new(tail.clone()))),
                        span,
                    )?;
                    self.unify(bt, &Type::Record(a_only, Some(Box::new(tail))), span)
                }
            }
            (Some(at), None) => self.unify(at, &Type::Record(b_only, None), span),
            (None, Some(bt)) => self.unify(bt, &Type::Record(a_only, None), span),
            (None, None) => Ok(()),
        }
    }
    fn generic_parameters(&mut self, generics: &[Generic]) -> Checked<Vec<(String, Type)>> {
        let mut result = Vec::new();
        let mut names = BTreeSet::new();
        for g in generics {
            if !names.insert(g.name.clone()) {
                return Err(self.error(0..0, format!("duplicate generic parameter `{}`", g.name)));
            }
            let kind = if let Some(k) = &g.kind {
                self.type_expr(k)?
            } else {
                Type::Universe(0)
            };
            self.parameter_kinds.insert(g.name.clone(), kind.clone());
            result.push((g.name.clone(), kind.clone()));
            self.parameters.insert(
                g.name.clone(),
                if matches!(kind, Type::Universe(_)) {
                    Type::Parameter(g.name.clone())
                } else {
                    Type::Index(g.name.clone(), Box::new(kind))
                },
            );
        }
        Ok(result)
    }
    fn type_expr(&mut self, syntax: &TypeExpr) -> Checked<Type> {
        match &syntax.node {
            TypeSyntax::Path(path, args) => {
                let ty = self.type_path(path, args, &syntax.span)?;
                if matches!(
                    ty,
                    Type::Index(..)
                        | Type::IntegerIndex(_)
                        | Type::BooleanIndex(_)
                        | Type::StringIndex(_)
                ) {
                    return Err(self.error(
                        syntax.span.clone(),
                        format!("static value {ty} cannot be used as a type"),
                    ));
                }
                Ok(ty)
            }
            TypeSyntax::Function(params, result) => {
                let mut fields = Vec::new();
                let mut names = BTreeSet::new();
                for p in params {
                    if !names.insert(&p.name) {
                        return Err(self.error(
                            syntax.span.clone(),
                            format!("duplicate parameter `{}`", p.name),
                        ));
                    }
                    let t = p.annotation.as_ref().ok_or_else(|| {
                        self.error(
                            syntax.span.clone(),
                            format!("function type parameter `{}` needs an annotation", p.name),
                        )
                    })?;
                    fields.push((p.name.clone(), self.type_expr(t)?));
                }
                Ok(Type::Function(fields, Box::new(self.type_expr(result)?)))
            }
            TypeSyntax::Tuple(fields) => {
                let mut labels = BTreeSet::new();
                for (label, _) in fields {
                    if let Some(label) = label
                        && !labels.insert(label)
                    {
                        return Err(self.error(
                            syntax.span.clone(),
                            format!("duplicate product field `{label}`"),
                        ));
                    }
                }
                Ok(Type::Tuple(
                    fields
                        .iter()
                        .map(|(n, t)| Ok((n.clone(), self.type_expr(t)?)))
                        .collect::<Checked<_>>()?,
                ))
            }
        }
    }
    fn type_path(&mut self, path: &Path, args: &[GenericArg], span: &Span) -> Checked<Type> {
        let text = path.text();
        if path.segments.len() == 1 {
            if let Some(t) = self.parameters.get(&text) {
                if !args.is_empty() {
                    return Err(self.error(
                        span.clone(),
                        "a generic parameter cannot have type arguments",
                    ));
                }
                return Ok(t.clone());
            }
            if let Some(t) = self.primitive_type(&text, args, span)? {
                return Ok(t);
            }
        }
        let (name, entry) = self
            .symbols
            .entry(&self.module, path)
            .ok_or_else(|| self.error(span.clone(), format!("unknown or private type `{text}`")))?;
        match &entry.kind {
            ItemKind::ExternalType(_, generics, _) | ItemKind::Enum(_, generics, _) => {
                let parameters =
                    self.with_entry(&entry, |this| this.generic_parameters(generics))?;
                let substitutions = self.generic_arguments(&parameters, args, span, false)?;
                Ok(Type::Nominal(
                    name,
                    parameters
                        .iter()
                        .map(|(n, _)| substitutions[n].clone())
                        .collect(),
                ))
            }
            ItemKind::Type(_, generics, body) => {
                if !self.aliases.insert(name.clone()) {
                    return Err(self.error(span.clone(), format!("recursive type alias `{name}`")));
                }
                let parameters =
                    self.with_entry(&entry, |this| this.generic_parameters(generics))?;
                let substitutions = self.generic_arguments(&parameters, args, span, false)?;
                let result = self.with_entry(&entry, |this| {
                    this.generic_parameters(generics)?;
                    this.type_expr(body)
                });
                self.aliases.remove(&name);
                result.map(|t| t.substitute(&substitutions))
            }
            _ => Err(self.error(span.clone(), format!("`{name}` does not denote a type"))),
        }
    }
    fn primitive_type(
        &mut self,
        name: &str,
        args: &[GenericArg],
        span: &Span,
    ) -> Checked<Option<Type>> {
        let scalar = match name {
            "Bool" => Some(Type::Bool),
            "Integer" => Some(Type::Integer),
            "Nat" => Some(Type::Nat),
            "String" => Some(Type::String),
            "Unit" => Some(Type::Tuple(Vec::new())),
            "Type" => Some(Type::Universe(0)),
            _ => name
                .strip_prefix("Type_")
                .and_then(|n| n.parse::<u32>().ok())
                .map(Type::Universe),
        };
        if let Some(t) = scalar {
            if !args.is_empty() {
                return Err(
                    self.error(span.clone(), format!("`{name}` takes no generic arguments"))
                );
            }
            return Ok(Some(t));
        }
        if matches!(name, "List" | "TypeRep" | "Named" | "Option") {
            if args.len() != 1 {
                return Err(
                    self.error(span.clone(), format!("`{name}` requires one type argument"))
                );
            }
            let t = self.generic_type(&args[0], span)?;
            return Ok(Some(match name {
                "List" => Type::List(Box::new(t)),
                "TypeRep" => Type::Description(Box::new(t)),
                "Named" => Type::Named(Box::new(t)),
                _ => Type::Nominal("Option".into(), vec![t]),
            }));
        }
        if name == "Record" {
            if let [argument] = args {
                let schema = match argument {
                    GenericArg::Value(schema) => schema.clone(),
                    GenericArg::Type(syntax) => match &syntax.node {
                        TypeSyntax::Path(path, args) => Expr {
                            span: syntax.span.clone(),
                            node: Expression::Path(path.clone(), args.clone()),
                        },
                        _ => {
                            return Err(
                                self.error(span.clone(), "Record argument must be a schema value")
                            );
                        }
                    },
                };
                return Ok(Some(self.schema_type(&schema)?));
            }
            return Err(self.error(
                span.clone(),
                "Record requires one finite field schema argument",
            ));
        }
        Ok(None)
    }
    fn schema_type(&mut self, schema: &Expr) -> Checked<Type> {
        if let Expression::Record(fields) = &schema.node {
            let mut row = BTreeMap::new();
            for (name, value) in fields {
                let decoded = match &value.node {
                    Expression::Description(syntax) => self.type_expr(syntax)?,
                    Expression::Path(path, args) if self.is_type_name(path) => {
                        self.type_path(path, args, &value.span)?
                    }
                    _ => {
                        let (_, ty) = self.infer(value, &mut Environment::new())?;
                        match self.resolve_type(&ty) {
                            Type::Description(t) => *t,
                            ty => return Err(self.error(
                                value.span.clone(),
                                format!(
                                    "schema field `{name}` requires a type description, found {ty}"
                                ),
                            )),
                        }
                    }
                };
                if matches!(
                    decoded,
                    Type::Index(..)
                        | Type::IntegerIndex(_)
                        | Type::BooleanIndex(_)
                        | Type::StringIndex(_)
                ) {
                    return Err(self.error(
                        value.span.clone(),
                        "a schema field must describe a type, not a static value",
                    ));
                }
                if row.insert(name.clone(), decoded).is_some() {
                    return Err(self.error(
                        value.span.clone(),
                        format!("duplicate schema field `{name}`"),
                    ));
                }
            }
            return Ok(Type::Record(row, None));
        }
        let (_, ty) = self.infer(schema, &mut Environment::new())?;
        match self.resolve_type(&ty) {
            Type::Record(fields, None) => {
                let mut row = BTreeMap::new();
                for (name, ty) in fields {
                    if let Type::Description(decoded) = ty {
                        row.insert(name, *decoded);
                    } else {
                        return Err(self.error(
                            schema.span.clone(),
                            format!(
                                "schema field `{name}` requires a type description, found {ty}"
                            ),
                        ));
                    }
                }
                Ok(Type::Record(row, None))
            }
            ty => Err(self.error(
                schema.span.clone(),
                format!("Record requires a finite closed schema, found {ty}"),
            )),
        }
    }
    fn is_type_name(&self, path: &Path) -> bool {
        self.parameters.contains_key(&path.text())
            || matches!(
                path.text().as_str(),
                "Bool"
                    | "Integer"
                    | "Nat"
                    | "String"
                    | "Unit"
                    | "List"
                    | "Record"
                    | "Named"
                    | "Option"
                    | "TypeRep"
                    | "Type"
            )
            || self
                .symbols
                .entry(&self.module, path)
                .is_some_and(|(_, e)| {
                    matches!(
                        e.kind,
                        ItemKind::Type(..) | ItemKind::Enum(..) | ItemKind::ExternalType(..)
                    )
                })
    }
    fn generic_type(&mut self, arg: &GenericArg, span: &Span) -> Checked<Type> {
        match arg {
            GenericArg::Type(t) => self.type_expr(t),
            _ => Err(self.error(span.clone(), "expected a type argument")),
        }
    }
    fn generic_arguments(
        &mut self,
        parameters: &[(String, Type)],
        args: &[GenericArg],
        span: &Span,
        infer: bool,
    ) -> Checked<Environment> {
        if args.len() > parameters.len() || (!infer && args.len() != parameters.len()) {
            return Err(self.error(
                span.clone(),
                format!(
                    "expected {} generic arguments, found {}",
                    parameters.len(),
                    args.len()
                ),
            ));
        }
        let mut result = BTreeMap::new();
        for (index, (name, kind)) in parameters.iter().enumerate() {
            let ty = match args.get(index) {
                Some(arg) if matches!(kind, Type::Universe(_)) => {
                    let ty = self.generic_type(arg, span)?;
                    if let (Type::Universe(level), Type::Universe(bound)) = (&ty, kind)
                        && level.checked_add(1).is_none_or(|level| level > *bound)
                    {
                        return Err(self.error(
                            span.clone(),
                            "generic argument exceeds its predicative universe kind",
                        ));
                    }
                    ty
                }
                Some(arg) => self.static_argument(arg, kind, span)?,
                None => {
                    let ty = self.fresh();
                    if let Type::Variable(v) = ty {
                        self.variable_kinds.insert(v, kind.clone());
                    }
                    ty
                }
            };
            if !matches!(ty, Type::Variable(_)) {
                self.check_argument_kind(&ty, kind, span)?;
            }
            result.insert(name.clone(), ty);
        }
        Ok(result)
    }
    fn check_argument_kind(&self, ty: &Type, kind: &Type, span: &Span) -> Checked<()> {
        let valid = match kind {
            Type::Universe(bound) => self
                .type_level(ty, &mut BTreeSet::new())
                .is_some_and(|level| level <= *bound),
            Type::Nat => {
                matches!(ty,Type::IntegerIndex(n) if *n>=0)
                    || matches!(ty,Type::Index(_,kind) if kind.as_ref()==&Type::Nat)
            }
            Type::Integer => {
                matches!(ty, Type::IntegerIndex(_))
                    || matches!(ty,Type::Index(_,kind) if kind.as_ref()==&Type::Integer)
            }
            Type::Bool => {
                matches!(ty, Type::BooleanIndex(_))
                    || matches!(ty,Type::Index(_,kind) if kind.as_ref()==&Type::Bool)
            }
            Type::String => {
                matches!(ty, Type::StringIndex(_))
                    || matches!(ty,Type::Index(_,kind) if kind.as_ref()==&Type::String)
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(self.error(
                span.clone(),
                format!("generic argument {ty} does not inhabit kind {kind}"),
            ))
        }
    }
    fn type_level(&self, ty: &Type, seen: &mut BTreeSet<String>) -> Option<u32> {
        let maximum = |types: Vec<&Type>, seen: &mut BTreeSet<String>| {
            types
                .into_iter()
                .map(|t| self.type_level(t, seen))
                .try_fold(0, |a, b| b.map(|b| a.max(b)))
        };
        match ty {
            Type::Universe(level) => level.checked_add(1),
            Type::Parameter(name) => match self.parameter_kinds.get(name) {
                Some(Type::Universe(level)) => Some(*level),
                _ => None,
            },
            Type::Variable(v) => match self.variable_kinds.get(v) {
                Some(Type::Universe(level)) => Some(*level),
                _ => Some(0),
            },
            Type::Index(..)
            | Type::IntegerIndex(_)
            | Type::BooleanIndex(_)
            | Type::StringIndex(_) => None,
            Type::Function(args, result) => maximum(
                args.iter()
                    .map(|(_, t)| t)
                    .chain(std::iter::once(result.as_ref()))
                    .collect(),
                seen,
            ),
            Type::Tuple(fields) => maximum(fields.iter().map(|(_, t)| t).collect(), seen),
            Type::Record(fields, tail) => maximum(
                fields
                    .values()
                    .chain(tail.iter().map(Box::as_ref))
                    .collect(),
                seen,
            ),
            Type::Description(t) | Type::List(t) | Type::Named(t) => self.type_level(t, seen),
            Type::Nominal(name, args) => {
                let mut level = self.nominal_levels.get(name).copied().unwrap_or(0);
                for arg in args {
                    if !matches!(
                        arg,
                        Type::Index(..)
                            | Type::IntegerIndex(_)
                            | Type::BooleanIndex(_)
                            | Type::StringIndex(_)
                    ) {
                        level = level.max(self.type_level(arg, seen)?);
                    }
                }
                if !seen.insert(name.clone()) {
                    return Some(level);
                }
                if let Some(info) = self.enums.get(name) {
                    let parameters = info
                        .parameters
                        .iter()
                        .zip(args)
                        .map(|((n, _), t)| (n.clone(), t.clone()))
                        .collect();
                    for field in info.variants.iter().flat_map(|(_, fields)| fields) {
                        level = level.max(self.type_level(&field.substitute(&parameters), seen)?);
                    }
                }
                seen.remove(name);
                Some(level)
            }
            _ => Some(0),
        }
    }
    fn static_argument(&mut self, arg: &GenericArg, kind: &Type, span: &Span) -> Checked<Type> {
        let value = match arg {
            GenericArg::Type(ast::Located {
                node: TypeSyntax::Path(p, a),
                ..
            }) if a.is_empty() => self.parameters.get(&p.text()).cloned(),
            GenericArg::Value(e) => match &e.node {
                Expression::Literal(Literal::Integer(n))
                    if matches!(kind, Type::Nat | Type::Integer)
                        && (*n >= 0 || kind == &Type::Integer) =>
                {
                    Some(Type::IntegerIndex(*n))
                }
                Expression::Literal(Literal::Bool(b)) if kind == &Type::Bool => {
                    Some(Type::BooleanIndex(*b))
                }
                Expression::Literal(Literal::String(s)) if kind == &Type::String => {
                    Some(Type::StringIndex(s.clone()))
                }
                _ => return self.normalize_static(e, kind, span),
            },
            _ => None,
        };
        match value {
            Some(Type::Index(n,k)) if k.as_ref()==kind=>Ok(Type::Index(n,k)),
            Some(t @ (Type::IntegerIndex(_)|Type::BooleanIndex(_)|Type::StringIndex(_)))=>Ok(t),
            _=>Err(self.error(span.clone(),format!("static argument must normalize to a value of {kind}; computed or mismatched indices are not supported"))),
        }
    }
    fn normalize_static(&mut self, expression: &Expr, kind: &Type, span: &Span) -> Checked<Type> {
        let start = self.obligations.len();
        let (mut term, ty) = self.infer(expression, &mut Environment::new())?;
        self.unify(&ty, kind, span)?;
        self.finish_constraints(start, false)?;
        term.map_types(&mut |t| self.resolve_type(t));
        let value = Evaluator::new(&self.definitions)
            .expression(&self.source, &term)
            .map_err(|e| {
                self.error(
                    span.clone(),
                    format!("static argument cannot normalize: {}", e.message),
                )
            })?;
        match (kind, value) {
            (Type::Integer, Value::Integer(n)) => Ok(Type::IntegerIndex(n)),
            (Type::Nat, Value::Integer(n)) if n >= 0 => Ok(Type::IntegerIndex(n)),
            (Type::Bool, Value::Bool(b)) => Ok(Type::BooleanIndex(b)),
            (Type::String, Value::String(s)) => Ok(Type::StringIndex(s)),
            _ => Err(self.error(
                span.clone(),
                format!("static value is not representable at kind {kind}"),
            )),
        }
    }
    fn with_entry<T>(
        &mut self,
        entry: &Entry,
        f: impl FnOnce(&mut Self) -> Checked<T>,
    ) -> Checked<T> {
        let source = std::mem::replace(&mut self.source, entry.source.clone());
        let module = std::mem::replace(&mut self.module, entry.module.clone());
        let parameters = std::mem::take(&mut self.parameters);
        let kinds = std::mem::take(&mut self.parameter_kinds);
        let result = f(self);
        self.source = source;
        self.module = module;
        self.parameters = parameters;
        self.parameter_kinds = kinds;
        result
    }
    fn prepare_enum(&mut self, name: &str, entry: &Entry) -> Checked<()> {
        if self.enums.contains_key(name) {
            return Ok(());
        }
        let ItemKind::Enum(_, generics, variants) = &entry.kind else {
            return Ok(());
        };
        self.with_entry(entry,|this|{
            let parameters=this.generic_parameters(generics)?;
            let arguments:Vec<_>=parameters.iter().map(|(n,k)|if matches!(k,Type::Universe(_)){Type::Parameter(n.clone())}else{Type::Index(n.clone(),Box::new(k.clone()))}).collect();
            let result=Type::Nominal(name.into(),arguments.clone());
            this.enums.insert(name.into(),EnumInfo {parameters:parameters.clone(),variants:Vec::new()});
            let mut constructors=Vec::new();let mut seen=BTreeSet::new();
            for variant in variants {
                if !variant.generics.is_empty()||variant.result.is_some(){return Err(this.error(entry.span.clone(),"indexed or constructor-generic enums require equality refinement; this compiler accepts uniform inductive parameters"));}
                if !seen.insert(&variant.name){return Err(this.error(entry.span.clone(),format!("duplicate constructor `{}`",variant.name)));}
                let fields=variant.fields.iter().map(|t|this.type_expr(t)).collect::<Checked<Vec<_>>>()?;
                for field in &fields{this.check_positive(name,&arguments,field,true,&entry.span)?;}
                let canonical=qualify(name,&variant.name);
                let ty=Type::Function(fields.iter().enumerate().map(|(i,t)|(format!("_{i}"),t.clone())).collect(),Box::new(result.clone()));
                let scheme=Scheme {parameters:parameters.clone(),ty,constraints:Vec::new()};
                this.register_constructor(entry,&canonical,&variant.name,fields.len(),scheme)?;
                constructors.push((canonical,fields));
            }
            this.enums.insert(name.into(),EnumInfo {parameters,variants:constructors});Ok(())
        })
    }
    fn register_constructor(
        &mut self,
        entry: &Entry,
        canonical: &str,
        variant: &str,
        arity: usize,
        scheme: Scheme,
    ) -> Checked<()> {
        let short = qualify(&entry.module, variant);
        if self
            .symbols
            .entries
            .get(&short)
            .is_some_and(|e| matches!(e.kind, ItemKind::Let { .. } | ItemKind::ExternalValue(..)))
            || self.symbols.entries.contains_key(canonical)
        {
            return Err(self.error(
                entry.span.clone(),
                format!("constructor `{canonical}` conflicts with a value declaration"),
            ));
        }
        let mut keys = vec![canonical.to_string()];
        if !crate::resolve::reserved_name(variant) && !self.symbols.entries.contains_key(&short) {
            keys.push(short);
        }
        for key in keys {
            if self.constructors.contains_key(&key) {
                return Err(
                    self.error(entry.span.clone(), format!("ambiguous constructor `{key}`"))
                );
            }
            self.constructors.insert(key.clone(), scheme.clone());
            self.definitions.insert(
                key,
                Definition {
                    source: entry.source.clone(),
                    namespace: None,
                    term: Term {
                        ty: scheme.ty.clone(),
                        span: entry.span.clone(),
                        kind: TermKind::Constructor(canonical.into(), arity),
                    },
                    scheme: scheme.clone(),
                },
            );
        }
        Ok(())
    }
    fn check_positive(
        &self,
        name: &str,
        parameters: &[Type],
        ty: &Type,
        positive: bool,
        span: &Span,
    ) -> Checked<()> {
        match ty {
            Type::Nominal(n, args) if n == name => {
                if !positive {
                    return Err(self.error(
                        span.clone(),
                        format!("non-positive recursive occurrence of `{name}`"),
                    ));
                }
                if args != parameters {
                    return Err(
                        self.error(span.clone(), "recursive enum parameters must be uniform")
                    );
                }
            }
            Type::Function(args, result) => {
                for (_, t) in args {
                    self.check_positive(name, parameters, t, false, span)?;
                }
                self.check_positive(name, parameters, result, positive, span)?;
            }
            Type::Nominal(_, args) => {
                for t in args {
                    if contains_nominal(t, name) {
                        return Err(self.error(span.clone(),"recursive occurrences beneath another nominal constructor require a positivity declaration"));
                    }
                }
            }
            _ => {
                let mut failure = None;
                ty.map(&mut |t| {
                    if t != ty && failure.is_none() {
                        failure = self
                            .check_positive(name, parameters, t, positive, span)
                            .err();
                    }
                    None
                });
                if let Some(e) = failure {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
    fn definition(&mut self, name: &str) -> Checked<Scheme> {
        if self.visiting.contains(name) {
            if self.active_definitions.last().map(String::as_str) != Some(name) {
                return Err(Diagnostic::new(
                    &self.source,
                    0..0,
                    Phase::Termination,
                    format!("unsupported cross-definition recursive cycle through `{name}`"),
                ));
            }
            return self.recursive_schemes.get(name).cloned().ok_or_else(|| {
                self.error(0..0, format!("cyclic non-recursive definition `{name}`"))
            });
        }
        if let Some(d) = self.definitions.get(name) {
            return Ok(d.scheme.clone());
        }
        let entry = self
            .symbols
            .entries
            .get(name)
            .cloned()
            .ok_or_else(|| self.error(0..0, format!("unknown value `{name}`")))?;
        self.visiting.insert(name.into());
        self.active_definitions.push(name.into());
        let result = self.with_entry(&entry, |this| this.check_definition(name, &entry));
        self.visiting.remove(name);
        self.active_definitions.pop();
        self.recursive_schemes.remove(name);
        result
    }
    fn check_definition(&mut self, name: &str, entry: &Entry) -> Checked<Scheme> {
        match &entry.kind {
            ItemKind::ExternalValue(_, generics, annotation) => {
                let parameters = self.generic_parameters(generics)?;
                let ty = self.type_expr(annotation)?;
                let scheme = Scheme {
                    parameters,
                    ty,
                    constraints: Vec::new(),
                };
                self.definitions.insert(
                    name.into(),
                    Definition {
                        source: entry.source.clone(),
                        namespace: self.namespace(entry),
                        term: Term {
                            ty: scheme.ty.clone(),
                            span: entry.span.clone(),
                            kind: if entry.module == "relational" {
                                TermKind::Trusted(name.rsplit("::").next().unwrap_or(name).into())
                            } else {
                                TermKind::Foreign(format!("{}::{name}", self.package))
                            },
                        },
                        scheme: scheme.clone(),
                    },
                );
                Ok(scheme)
            }
            ItemKind::Let {
                path,
                generics,
                recursive,
                annotation,
                value,
            } => {
                let mut all_generics = generics.clone();
                if let Expression::Function {
                    generics: function_generics,
                    ..
                } = &value.node
                {
                    all_generics.extend(function_generics.clone());
                }
                let parameters = self.generic_parameters(&all_generics)?;
                let start = self.obligations.len();
                let expected = annotation.as_ref().map(|t| self.type_expr(t)).transpose()?;
                let mut environment = Environment::new();
                let provisional = if *recursive {
                    Some(self.function_signature(value, expected.as_ref())?)
                } else {
                    None
                };
                if let Some(ty) = &provisional {
                    self.recursive_schemes.insert(
                        name.into(),
                        Scheme {
                            parameters: parameters.clone(),
                            ty: ty.clone(),
                            constraints: Vec::new(),
                        },
                    );
                }
                let (mut term, mut ty) = self.infer(value, &mut environment)?;
                if let Some(expected) = &expected {
                    self.unify(&ty, expected, &value.span)?;
                    ty = expected.clone();
                }
                if let Some(provisional) = &provisional {
                    self.unify(&ty, provisional, &value.span)?;
                    self.check_recursion(name, &term)?;
                }
                let constraints = if self.defer_completion {
                    self.obligations
                        .split_off(start)
                        .into_iter()
                        .map(|obligation| obligation.constraint)
                        .collect()
                } else {
                    self.finish_constraints(start, !parameters.is_empty())?
                };
                ty = self.resolve_type(&ty);
                term.map_types(&mut |t| self.resolve_type(t));
                if !self.defer_completion {
                    self.reject_ambiguity(&ty, &term)?;
                }
                if path.segments.len() > 1 {
                    ty = Type::Named(Box::new(ty));
                    term = Term {
                        ty: ty.clone(),
                        span: term.span.clone(),
                        kind: TermKind::Named(format!("{}::{name}", self.package), Box::new(term)),
                    };
                }
                let scheme = Scheme {
                    parameters,
                    ty,
                    constraints,
                };
                self.definitions.insert(
                    name.into(),
                    Definition {
                        source: entry.source.clone(),
                        namespace: self.namespace(entry),
                        term,
                        scheme: scheme.clone(),
                    },
                );
                Ok(scheme)
            }
            _ => Err(self.error(entry.span.clone(), format!("`{name}` is not a value"))),
        }
    }
    fn reject_ambiguity(&self, ty: &Type, term: &Term) -> Checked<()> {
        reject_ambiguous(ty).map_err(|ty| {
            self.error(
                term.span.clone(),
                format!(
                    "unsolved type {ty}; annotate the binding or add an explicit generic parameter"
                ),
            )
        })?;
        reject_unsolved_arguments(term).map_err(|span| {
            self.error(
                span,
                "generic arguments remain ambiguous; supply explicit arguments",
            )
        })
    }
    fn function_signature(&mut self, expr: &Expr, expected: Option<&Type>) -> Checked<Type> {
        if let Some(t) = expected
            && matches!(t, Type::Function(..))
        {
            return Ok(t.clone());
        }
        if let Expression::Function {
            parameters, result, ..
        } = &expr.node
        {
            let parameters = parameters
                .iter()
                .map(|p| {
                    Ok((
                        p.name.clone(),
                        p.annotation
                            .as_ref()
                            .map(|t| self.type_expr(t))
                            .transpose()?
                            .unwrap_or_else(|| self.fresh()),
                    ))
                })
                .collect::<Checked<_>>()?;
            let result = result
                .as_ref()
                .map(|t| self.type_expr(t))
                .transpose()?
                .unwrap_or_else(|| self.fresh());
            Ok(Type::Function(parameters, Box::new(result)))
        } else {
            Err(self.error(expr.span.clone(), "let rec requires an ordinary function"))
        }
    }
    fn instantiate(
        &mut self,
        scheme: &Scheme,
        args: &[GenericArg],
        span: &Span,
    ) -> Checked<(Type, Environment, BTreeMap<u32, Type>)> {
        let parameters = self.generic_arguments(&scheme.parameters, args, span, true)?;
        let mut variables = BTreeMap::new();
        for v in scheme.ty.variables().into_iter().chain(
            scheme
                .constraints
                .iter()
                .flat_map(|c| c.arguments.iter().flat_map(Type::variables)),
        ) {
            variables.entry(v).or_insert_with(|| self.fresh());
        }
        let mut specialize = |ty: &Type| {
            ty.substitute(&parameters).map(&mut |t| {
                if let Type::Variable(v) = t {
                    variables.get(v).cloned()
                } else {
                    None
                }
            })
        };
        let ty = specialize(&scheme.ty);
        for constraint in &scheme.constraints {
            self.obligations.push(Obligation {
                constraint: Constraint {
                    class: constraint.class.clone(),
                    arguments: constraint.arguments.iter().map(&mut specialize).collect(),
                },
                span: span.clone(),
            });
        }
        Ok((ty, parameters, variables))
    }
    fn obligation(&mut self, class: &str, arguments: Vec<Type>, span: &Span) {
        self.obligations.push(Obligation {
            constraint: Constraint {
                class: class.into(),
                arguments,
            },
            span: span.clone(),
        });
    }
    fn finish_constraints(&mut self, start: usize, generic: bool) -> Checked<Vec<Constraint>> {
        let obligations = self.obligations.split_off(start);
        for obligation in &obligations {
            if obligation.constraint.class == "FromInteger"
                && let Some(Type::Variable(v)) = obligation
                    .constraint
                    .arguments
                    .first()
                    .map(|t| self.resolve_type(t))
            {
                self.bind(v, &Type::Integer, &obligation.span)?;
            }
        }
        let mut result = Vec::new();
        for obligation in obligations {
            let constraint = Constraint {
                class: obligation.constraint.class,
                arguments: obligation
                    .constraint
                    .arguments
                    .iter()
                    .map(|t| self.resolve_type(t))
                    .collect(),
            };
            if self.constraint_supported(&constraint) {
                continue;
            }
            if generic && constraint.arguments.iter().any(has_parameter) {
                if !result.contains(&constraint) {
                    result.push(constraint);
                }
                continue;
            }
            return Err(self.error(
                obligation.span,
                format!("no coherent dictionary for {constraint}"),
            ));
        }
        Ok(result)
    }
    fn constraint_supported(&self, c: &Constraint) -> bool {
        match (c.class.as_str(), c.arguments.as_slice()) {
            ("FromInteger", [Type::Integer, Type::IntegerIndex(_)]) => true,
            ("FromInteger", [Type::Nat, Type::IntegerIndex(n)]) => *n >= 0,
            ("Describe", [t]) => describable(t),
            ("Not", [Type::Bool]) => true,
            ("Neg", [Type::Integer]) => true,
            ("Eq", [Type::Bool | Type::String | Type::Integer | Type::Nat]) => true,
            ("Ord", [Type::String | Type::Integer | Type::Nat]) => true,
            ("BitAnd" | "BitOr" | "BitXor", [a, b, c]) => {
                a == b && b == c && matches!(a, Type::Bool | Type::Integer | Type::Nat)
            }
            ("Add" | "Sub" | "Mul" | "Div" | "Rem" | "Shl" | "Shr", [a, b, c]) => {
                a == b && b == c && matches!(a, Type::Integer | Type::Nat)
            }
            _ => false,
        }
    }
    fn infer(&mut self, expr: &Expr, environment: &mut Environment) -> Checked<(Term, Type)> {
        let (kind, ty) = match &expr.node {
            Expression::Literal(literal) => {
                let ty = match literal {
                    Literal::Integer(n) => {
                        let ty = self.fresh();
                        self.obligation(
                            "FromInteger",
                            vec![ty.clone(), Type::IntegerIndex(*n)],
                            &expr.span,
                        );
                        ty
                    }
                    Literal::Bool(_) => Type::Bool,
                    Literal::String(_) => Type::String,
                };
                (TermKind::Literal(literal.clone()), ty)
            }
            Expression::Path(path, args) => return self.infer_path(expr, path, args, environment),
            Expression::Description(syntax) => {
                let ty = self.type_expr(syntax)?;
                self.obligation("Describe", vec![ty.clone()], &expr.span);
                (
                    TermKind::Description(ty.clone()),
                    Type::Description(Box::new(ty)),
                )
            }
            Expression::Function { .. } => return self.infer_function(expr, environment),
            Expression::Call(function, args) => {
                return self.infer_call(expr, function, args, environment);
            }
            Expression::Project(value, field) => {
                return self.infer_projection(expr, value, field, environment);
            }
            Expression::Index(value, index) => {
                let (value, ty) = self.infer(value, environment)?;
                let item = self.fresh();
                self.unify(&ty, &Type::List(Box::new(item.clone())), &expr.span)?;
                let (index, index_type) = self.infer(index, environment)?;
                self.unify(&index_type, &Type::Nat, &expr.span)?;
                (
                    TermKind::Index(Box::new(value), Box::new(index)),
                    Type::Nominal("Option".into(), vec![item]),
                )
            }
            Expression::Record(fields) => return self.infer_record(expr, fields, environment),
            Expression::Tuple(fields) => {
                let fields = fields
                    .iter()
                    .map(|e| self.infer(e, environment))
                    .collect::<Checked<Vec<_>>>()?;
                let ty = Type::Tuple(fields.iter().map(|(_, t)| (None, t.clone())).collect());
                (
                    TermKind::Tuple(fields.into_iter().map(|(t, _)| t).collect()),
                    ty,
                )
            }
            Expression::List(fields) => {
                let mut item = self.fresh();
                let mut terms = Vec::new();
                for e in fields {
                    let (term, ty) = self.infer(e, environment)?;
                    if let Some(common) = crate::relations::common_type(
                        &self.resolve_type(&item),
                        &self.resolve_type(&ty),
                    ) {
                        item = common;
                    } else {
                        self.unify(&ty, &item, &e.span)?;
                    }
                    terms.push(term);
                }
                (TermKind::List(terms), Type::List(Box::new(item)))
            }
            Expression::Block(locals, body) => {
                return self.infer_block(expr, locals, body, environment);
            }
            Expression::If(condition, yes, no) => {
                let (condition, condition_ty) = self.infer(condition, environment)?;
                self.unify(&condition_ty, &Type::Bool, &expr.span)?;
                let (yes, yes_ty) = self.infer(yes, environment)?;
                let (no, no_ty) = self.infer(no, environment)?;
                self.unify(&yes_ty, &no_ty, &expr.span)?;
                (
                    TermKind::If(Box::new(condition), Box::new(yes), Box::new(no)),
                    yes_ty,
                )
            }
            Expression::Match(scrutinee, arms) => {
                return self.infer_match(expr, scrutinee, arms, environment);
            }
            Expression::Unary(op, value) => {
                let (value, ty) = self.infer(value, environment)?;
                self.obligation(
                    if op == "!" { "Not" } else { "Neg" },
                    vec![ty.clone()],
                    &expr.span,
                );
                (TermKind::Unary(op.clone(), Box::new(value)), ty)
            }
            Expression::Binary(op, left, right) => {
                return self.infer_binary(expr, op, left, right, environment);
            }
        };
        Ok((
            Term {
                ty: ty.clone(),
                span: expr.span.clone(),
                kind,
            },
            ty,
        ))
    }
    fn infer_path(
        &mut self,
        expr: &Expr,
        path: &Path,
        args: &[GenericArg],
        environment: &Environment,
    ) -> Checked<(Term, Type)> {
        let text = path.text();
        if path.segments.len() == 1 {
            if let Some(ty) = environment.get(&text) {
                if !args.is_empty() {
                    return Err(self.error(expr.span.clone(),"local bindings are monomorphic; explicit generic arguments are not allowed"));
                }
                return Ok((
                    Term {
                        ty: ty.clone(),
                        span: expr.span.clone(),
                        kind: TermKind::Local(text),
                    },
                    ty.clone(),
                ));
            }
            if self.parameters.contains_key(&text)
                || self.primitive_type(&text, args, &expr.span)?.is_some()
            {
                let ty = self.type_path(path, args, &expr.span)?;
                if let Type::Index(_, kind) = &ty {
                    return Ok((
                        Term {
                            ty: kind.as_ref().clone(),
                            span: expr.span.clone(),
                            kind: TermKind::Local(text),
                        },
                        kind.as_ref().clone(),
                    ));
                }
                self.obligation("Describe", vec![ty.clone()], &expr.span);
                return Ok((
                    Term {
                        ty: Type::Description(Box::new(ty.clone())),
                        span: expr.span.clone(),
                        kind: TermKind::Description(ty.clone()),
                    },
                    Type::Description(Box::new(ty)),
                ));
            }
        }
        if matches!(
            text.as_str(),
            "Some" | "None" | "Option::Some" | "Option::None"
        ) {
            let t = self.fresh();
            let result = Type::Nominal("Option".into(), vec![t.clone()]);
            let count = usize::from(text.ends_with("Some"));
            if !args.is_empty() {
                if args.len() != 1 {
                    return Err(self.error(
                        expr.span.clone(),
                        "Option constructor takes one type argument",
                    ));
                }
                let a = self.generic_type(&args[0], &expr.span)?;
                self.unify(&t, &a, &expr.span)?;
            }
            return Ok((
                Term {
                    ty: Type::Function(
                        if count == 1 {
                            vec![("_0".into(), t.clone())]
                        } else {
                            Vec::new()
                        },
                        Box::new(result.clone()),
                    ),
                    span: expr.span.clone(),
                    kind: TermKind::Constructor(
                        if count == 1 {
                            "Option::Some"
                        } else {
                            "Option::None"
                        }
                        .into(),
                        count,
                    ),
                },
                Type::Function(
                    if count == 1 {
                        vec![("_0".into(), t)]
                    } else {
                        Vec::new()
                    },
                    Box::new(result),
                ),
            ));
        }
        if let Some((name, entry)) = self.symbols.entry(&self.module, path) {
            if matches!(
                entry.kind,
                ItemKind::Type(..) | ItemKind::Enum(..) | ItemKind::ExternalType(..)
            ) {
                let ty = self.type_path(path, args, &expr.span)?;
                self.obligation("Describe", vec![ty.clone()], &expr.span);
                return Ok((
                    Term {
                        ty: Type::Description(Box::new(ty.clone())),
                        span: expr.span.clone(),
                        kind: TermKind::Description(ty.clone()),
                    },
                    Type::Description(Box::new(ty)),
                ));
            }
            let scheme = self.definition(&name)?;
            let (ty, parameters, rows) = self.instantiate(&scheme, args, &expr.span)?;
            return Ok((
                Term {
                    ty: ty.clone(),
                    span: expr.span.clone(),
                    kind: TermKind::Global(name, parameters, rows),
                },
                ty,
            ));
        }
        let constructor = self
            .symbols
            .resolve(&self.module, path)
            .into_iter()
            .chain([qualify(&self.module, &text), text.clone()])
            .find(|n| self.constructors.contains_key(n));
        if let Some(name) = constructor {
            if let Some(TermKind::Constructor(canonical, _)) =
                self.definitions.get(&name).map(|d| &d.term.kind)
                && let Some((owner, _)) = canonical.rsplit_once("::")
            {
                let owner_path = Path {
                    absolute: true,
                    segments: owner.split("::").map(str::to_string).collect(),
                };
                if self.symbols.entry(&self.module, &owner_path).is_none() {
                    return Err(self.error(
                        expr.span.clone(),
                        format!("constructor `{canonical}` is private"),
                    ));
                }
            }
            let scheme = self.constructors[&name].clone();
            let (ty, parameters, rows) = self.instantiate(&scheme, args, &expr.span)?;
            return Ok((
                Term {
                    ty: ty.clone(),
                    span: expr.span.clone(),
                    kind: TermKind::Global(name, parameters, rows),
                },
                ty,
            ));
        }
        Err(self.error(
            expr.span.clone(),
            format!("unknown or private value `{text}`"),
        ))
    }
    fn infer_function(&mut self, expr: &Expr, environment: &Environment) -> Checked<(Term, Type)> {
        let Expression::Function {
            generics,
            parameters,
            result,
            body,
        } = &expr.node
        else {
            return Err(self.error(expr.span.clone(), "expected function"));
        };
        if generics
            .iter()
            .any(|g| !self.parameters.contains_key(&g.name))
        {
            return Err(self.error(
                expr.span.clone(),
                "generic function binders must be at a top-level binding boundary",
            ));
        }
        let mut local = environment.clone();
        let mut names = BTreeSet::new();
        let mut types = Vec::new();
        for parameter in parameters {
            if !names.insert(&parameter.name) {
                return Err(self.error(
                    expr.span.clone(),
                    format!("duplicate parameter `{}`", parameter.name),
                ));
            }
            let ty = parameter
                .annotation
                .as_ref()
                .map(|t| self.type_expr(t))
                .transpose()?
                .unwrap_or_else(|| self.fresh());
            local.insert(parameter.name.clone(), ty.clone());
            types.push((parameter.name.clone(), ty));
        }
        let expected = result.as_ref().map(|t| self.type_expr(t)).transpose()?;
        let (body, result) = self.infer(body, &mut local)?;
        if let Some(expected) = expected {
            self.unify(&result, &expected, &expr.span)?;
        }
        Ok((
            Term {
                ty: Type::Function(types.clone(), Box::new(result.clone())),
                span: expr.span.clone(),
                kind: TermKind::Function(
                    parameters.iter().map(|p| p.name.clone()).collect(),
                    Box::new(body),
                ),
            },
            Type::Function(types, Box::new(result)),
        ))
    }
    fn unify_argument(&mut self, actual: &Type, expected: &Type, span: &Span) -> Checked<bool> {
        let actual = self.resolve_type(actual);
        let expected = self.resolve_type(expected);
        if crate::relations::coerces(&actual, &expected) {
            return Ok(true);
        }
        if crate::relations::expression_sort(&actual).is_none()
            && let Some(sort) = crate::relations::expression_sort(&expected)
        {
            self.unify(&actual, sort, span)?;
            return Ok(true);
        }
        match (&actual, &expected) {
            (Type::List(actual), Type::List(expected)) => {
                self.unify_argument(actual, expected, span)
            }
            (Type::Record(actual, None), Type::Record(expected, None))
                if actual.keys().eq(expected.keys()) =>
            {
                let mut converted = false;
                for (name, expected) in expected {
                    converted |= self.unify_argument(&actual[name], expected, span)?;
                }
                Ok(converted)
            }
            _ => {
                self.unify(&actual, &expected, span)?;
                Ok(false)
            }
        }
    }
    fn infer_call(
        &mut self,
        expr: &Expr,
        function: &Expr,
        args: &[(Option<String>, Expr)],
        environment: &mut Environment,
    ) -> Checked<(Term, Type)> {
        let (function, ty) = self.infer(function, environment)?;
        let ty = self.resolve_type(&ty);
        let ty = crate::relations::call_type(&ty).unwrap_or(ty);
        let (parameters, result) = match ty {
            Type::Function(p, r) => (p, *r),
            Type::Variable(v) => {
                let p = args
                    .iter()
                    .enumerate()
                    .map(|(i, (n, _))| (n.clone().unwrap_or_else(|| format!("_{i}")), self.fresh()))
                    .collect::<Vec<_>>();
                let result = self.fresh();
                self.bind(
                    v,
                    &Type::Function(p.clone(), Box::new(result.clone())),
                    &expr.span,
                )?;
                (p, result)
            }
            t => {
                return Err(self.error(expr.span.clone(), format!("cannot call value of type {t}")));
            }
        };
        if parameters.len() != args.len() {
            return Err(self.error(
                expr.span.clone(),
                format!(
                    "function arity: expected {}, found {}",
                    parameters.len(),
                    args.len()
                ),
            ));
        }
        let mut terms = vec![None; parameters.len()];
        let mut labelled = false;
        let mut ordered = Vec::new();
        for (position, (label, arg)) in args.iter().enumerate() {
            let index = if let Some(label) = label {
                labelled = true;
                parameters
                    .iter()
                    .position(|(n, _)| n == label)
                    .ok_or_else(|| {
                        self.error(
                            arg.span.clone(),
                            format!("unknown argument label `{label}`"),
                        )
                    })?
            } else {
                if labelled {
                    return Err(self.error(
                        arg.span.clone(),
                        "positional argument after labelled argument",
                    ));
                }
                position
            };
            if terms[index].is_some() {
                return Err(self.error(
                    arg.span.clone(),
                    format!("duplicate argument for `{}`", parameters[index].0),
                ));
            }
            let (mut term, ty) = self.infer(arg, environment)?;
            if self.unify_argument(&ty, &parameters[index].1, &arg.span)? {
                term = Term {
                    span: arg.span.clone(),
                    ty: parameters[index].1.clone(),
                    kind: TermKind::Coerce(Box::new(term)),
                };
            }
            ordered.push((index, term.clone()));
            terms[index] = Some(term);
        }
        // Preserve source evaluation order even when labels reorder parameters.
        let mut call = Term {
            ty: result.clone(),
            span: expr.span.clone(),
            kind: TermKind::Call(
                Box::new(function),
                terms
                    .into_iter()
                    .enumerate()
                    .map(|(i, t)| {
                        t.unwrap_or(Term {
                            ty: parameters[i].1.clone(),
                            span: expr.span.clone(),
                            kind: TermKind::Local(format!("$arg{i}")),
                        })
                    })
                    .collect(),
            ),
        };
        if ordered
            .iter()
            .enumerate()
            .any(|(i, (index, _))| i != *index)
            && let TermKind::Call(function, _) = call.kind
        {
            let function_name = format!("$function{}", expr.span.start);
            let original_function = function;
            let function = Box::new(Term {
                ty: Type::Function(parameters.clone(), Box::new(result.clone())),
                span: expr.span.clone(),
                kind: TermKind::Local(function_name.clone()),
            });
            let arguments = (0..parameters.len())
                .map(|i| Term {
                    ty: parameters[i].1.clone(),
                    span: expr.span.clone(),
                    kind: TermKind::Local(format!("$arg{}:{i}", expr.span.start)),
                })
                .collect();
            call = Term {
                ty: result.clone(),
                span: expr.span.clone(),
                kind: TermKind::Call(function, arguments),
            };
            for (index, term) in ordered.into_iter().rev() {
                call = Term {
                    ty: result.clone(),
                    span: expr.span.clone(),
                    kind: TermKind::Let(
                        Pattern::Bind(format!("$arg{}:{index}", expr.span.start)),
                        Box::new(term),
                        Box::new(call),
                    ),
                };
            }
            call = Term {
                ty: result.clone(),
                span: expr.span.clone(),
                kind: TermKind::Let(
                    Pattern::Bind(function_name),
                    original_function,
                    Box::new(call),
                ),
            };
        }
        Ok((call, result))
    }
    fn infer_projection(
        &mut self,
        expr: &Expr,
        value: &Expr,
        field: &str,
        environment: &mut Environment,
    ) -> Checked<(Term, Type)> {
        let (value, ty) = self.infer(value, environment)?;
        let ty = self.resolve_type(&ty);
        let result = match &ty {
            ty if crate::relations::projection_type(ty, field).is_some() => {
                crate::relations::projection_type(ty, field).unwrap_or_else(|| ty.clone())
            }
            Type::Tuple(fields) => {
                let index = field.parse::<usize>().ok();
                if index
                    .and_then(|i| fields.get(i))
                    .is_some_and(|(label, _)| label.is_some())
                {
                    return Err(self.error(
                        expr.span.clone(),
                        "numeric projections apply only to unlabelled product fields",
                    ));
                }
                fields
                    .iter()
                    .enumerate()
                    .find(|(i, (n, _))| index == Some(*i) || n.as_deref() == Some(field))
                    .map(|(_, (_, t))| t.clone())
                    .ok_or_else(|| {
                        self.error(expr.span.clone(), format!("tuple has no field `{field}`"))
                    })?
            }
            Type::Named(payload) if field == "payload" => payload.as_ref().clone(),
            Type::Named(_) if field == "identity" => Type::String,
            Type::Record(fields, None) => fields.get(field).cloned().ok_or_else(|| {
                self.error(expr.span.clone(), format!("record has no field `{field}`"))
            })?,
            Type::Record(..) | Type::Variable(_) => {
                let result = self.fresh();
                let tail = self.fresh();
                self.unify(
                    &ty,
                    &Type::Record(
                        BTreeMap::from([(field.into(), result.clone())]),
                        Some(Box::new(tail)),
                    ),
                    &expr.span,
                )?;
                result
            }
            _ => {
                return Err(self.error(
                    expr.span.clone(),
                    format!("type {ty} has no field `{field}`"),
                ));
            }
        };
        Ok((
            Term {
                ty: result.clone(),
                span: expr.span.clone(),
                kind: TermKind::Project(Box::new(value), field.into()),
            },
            result,
        ))
    }
    fn infer_record(
        &mut self,
        expr: &Expr,
        fields: &[(String, Expr)],
        environment: &mut Environment,
    ) -> Checked<(Term, Type)> {
        let mut terms = Vec::new();
        let mut types = BTreeMap::new();
        for (name, value) in fields {
            let (term, ty) = self.infer(value, environment)?;
            if types.insert(name.clone(), ty).is_some() {
                return Err(self.error(
                    value.span.clone(),
                    format!("duplicate record field `{name}`"),
                ));
            }
            terms.push((name.clone(), term));
        }
        Ok((
            Term {
                ty: Type::Record(types.clone(), None),
                span: expr.span.clone(),
                kind: TermKind::Record(terms),
            },
            Type::Record(types, None),
        ))
    }
    fn infer_binary(
        &mut self,
        expr: &Expr,
        operator: &str,
        left: &Expr,
        right: &Expr,
        environment: &mut Environment,
    ) -> Checked<(Term, Type)> {
        let (left, left_ty) = self.infer(left, environment)?;
        let (right, right_ty) = self.infer(right, environment)?;
        if let Some((operand, result)) =
            crate::relations::binary_type(operator, &self.resolve_type(&left_ty))
        {
            let expected =
                if crate::relations::expression_sort(&self.resolve_type(&right_ty)).is_none() {
                    crate::relations::expression_sort(&operand).unwrap_or(&operand)
                } else {
                    &operand
                };
            self.unify(&right_ty, expected, &expr.span)?;
            return Ok((
                Term {
                    ty: result.clone(),
                    span: expr.span.clone(),
                    kind: TermKind::Binary(
                        operator.into(),
                        Box::new(left),
                        Box::new(right),
                        left_ty,
                    ),
                },
                result,
            ));
        }
        self.unify(&left_ty, &right_ty, &expr.span)?;
        let result = if matches!(operator, "&&" | "||") {
            self.unify(&left_ty, &Type::Bool, &expr.span)?;
            Type::Bool
        } else {
            let class = match operator {
                "+" => "Add",
                "-" => "Sub",
                "*" => "Mul",
                "/" => "Div",
                "%" => "Rem",
                "<<" => "Shl",
                ">>" => "Shr",
                "&" => "BitAnd",
                "|" => "BitOr",
                "^" => "BitXor",
                "==" | "!=" => "Eq",
                _ => "Ord",
            };
            let arguments = if matches!(class, "Eq" | "Ord") {
                vec![left_ty.clone()]
            } else {
                vec![left_ty.clone(), right_ty, left_ty.clone()]
            };
            self.obligation(class, arguments, &expr.span);
            if matches!(class, "Eq" | "Ord") {
                Type::Bool
            } else {
                left_ty.clone()
            }
        };
        Ok((
            Term {
                ty: result.clone(),
                span: expr.span.clone(),
                kind: TermKind::Binary(
                    operator.into(),
                    Box::new(left),
                    Box::new(right),
                    left_ty.clone(),
                ),
            },
            result,
        ))
    }
    fn infer_block(
        &mut self,
        expr: &Expr,
        locals: &[ast::Local],
        body: &Expr,
        environment: &Environment,
    ) -> Checked<(Term, Type)> {
        let mut local = environment.clone();
        let mut bindings = Vec::new();
        for binding in locals {
            if !binding.generics.is_empty() {
                return Err(self.error(
                    binding.value.span.clone(),
                    "local let bindings are monomorphic",
                ));
            }
            let expected = binding
                .annotation
                .as_ref()
                .map(|t| self.type_expr(t))
                .transpose()?;
            if binding.recursive {
                let PatternSyntax::Bind(name) = &binding.pattern.node else {
                    return Err(self.error(
                        binding.pattern.span.clone(),
                        "recursive local binding must bind one name",
                    ));
                };
                let signature = self.function_signature(&binding.value, expected.as_ref())?;
                local.insert(name.clone(), signature.clone());
                let (term, ty) = self.infer(&binding.value, &mut local)?;
                self.unify(&ty, &signature, &binding.value.span)?;
                self.check_recursion(name, &term)?;
                bindings.push((Pattern::Bind(name.clone()), term, true));
            } else {
                let (term, ty) = self.infer(&binding.value, &mut local)?;
                if let Some(expected) = &expected {
                    self.unify(&ty, expected, &binding.value.span)?;
                }
                let mut names = BTreeSet::new();
                let pattern = self.check_pattern(&binding.pattern, &ty, &mut local, &mut names)?;
                if !self.patterns_exhaustive(&self.resolve_type(&ty), &[vec![pattern.clone()]]) {
                    return Err(self.error(
                        binding.pattern.span.clone(),
                        "local binding pattern is not exhaustive",
                    ));
                }
                bindings.push((pattern, term, false));
            }
        }
        let (mut term, ty) = self.infer(body, &mut local)?;
        for (pattern, value, recursive) in bindings.into_iter().rev() {
            let kind = if recursive {
                let Pattern::Bind(name) = pattern else {
                    return Err(self.error(expr.span.clone(), "invalid recursive binding"));
                };
                let TermKind::Function(parameters, body) = value.kind else {
                    return Err(self.error(value.span, "recursive binding requires a function"));
                };
                TermKind::LetRec(name, parameters, body, Box::new(term))
            } else {
                TermKind::Let(pattern, Box::new(value), Box::new(term))
            };
            term = Term {
                ty: ty.clone(),
                span: expr.span.clone(),
                kind,
            };
        }
        Ok((term, ty))
    }
    fn check_pattern(
        &mut self,
        pattern: &ast::Pattern,
        expected: &Type,
        environment: &mut Environment,
        names: &mut BTreeSet<String>,
    ) -> Checked<Pattern> {
        match &pattern.node {
            PatternSyntax::Wildcard => Ok(Pattern::Wildcard),
            PatternSyntax::Bind(n) => {
                if !names.insert(n.clone()) {
                    return Err(self.error(
                        pattern.span.clone(),
                        format!("duplicate pattern binding `{n}`"),
                    ));
                }
                environment.insert(n.clone(), expected.clone());
                Ok(Pattern::Bind(n.clone()))
            }
            PatternSyntax::Literal(literal) => {
                let literal_type = match literal {
                    Literal::Bool(_) => Type::Bool,
                    Literal::String(_) => Type::String,
                    Literal::Integer(n) => {
                        let t = self.fresh();
                        self.obligation(
                            "FromInteger",
                            vec![t.clone(), Type::IntegerIndex(*n)],
                            &pattern.span,
                        );
                        t
                    }
                };
                self.unify(&literal_type, expected, &pattern.span)?;
                Ok(Pattern::Literal(literal.clone()))
            }
            PatternSyntax::Constructor(path, fields) => {
                self.check_constructor_pattern(pattern, path, fields, expected, environment, names)
            }
            PatternSyntax::Tuple(fields) => {
                let types = fields.iter().map(|_| self.fresh()).collect::<Vec<_>>();
                self.unify(
                    expected,
                    &Type::Tuple(types.iter().map(|t| (None, t.clone())).collect()),
                    &pattern.span,
                )?;
                Ok(Pattern::Tuple(
                    fields
                        .iter()
                        .zip(types)
                        .map(|(p, t)| self.check_pattern(p, &t, environment, names))
                        .collect::<Checked<_>>()?,
                ))
            }
            PatternSyntax::Record(fields) => {
                let mut types = BTreeMap::new();
                for (n, _) in fields {
                    if types.insert(n.clone(), self.fresh()).is_some() {
                        return Err(self.error(
                            pattern.span.clone(),
                            format!("duplicate pattern field `{n}`"),
                        ));
                    }
                }
                let tail = self.fresh();
                self.unify(
                    expected,
                    &Type::Record(types.clone(), Some(Box::new(tail))),
                    &pattern.span,
                )?;
                Ok(Pattern::Record(
                    fields
                        .iter()
                        .map(|(n, p)| {
                            Ok((
                                n.clone(),
                                self.check_pattern(p, &types[n], environment, names)?,
                            ))
                        })
                        .collect::<Checked<_>>()?,
                ))
            }
        }
    }
    fn check_constructor_pattern(
        &mut self,
        pattern: &ast::Pattern,
        path: &Path,
        fields: &[ast::Pattern],
        expected: &Type,
        environment: &mut Environment,
        names: &mut BTreeSet<String>,
    ) -> Checked<Pattern> {
        let expression = Expr {
            span: pattern.span.clone(),
            node: Expression::Path(path.clone(), Vec::new()),
        };
        let (term, ty) = self.infer_path(&expression, path, &[], &Environment::new())?;
        let canonical = match term.kind {
            TermKind::Constructor(n, _) => n,
            TermKind::Global(n, _, _) => match self.definitions.get(&n).map(|d| &d.term.kind) {
                Some(TermKind::Constructor(canonical, _)) => canonical.clone(),
                _ => {
                    return Err(self.error(
                        pattern.span.clone(),
                        "pattern path is not an inductive constructor",
                    ));
                }
            },
            _ => {
                return Err(self.error(
                    pattern.span.clone(),
                    "pattern path is not an inductive constructor",
                ));
            }
        };
        let Type::Function(parameters, result) = ty else {
            return Err(self.error(pattern.span.clone(), "constructor is not callable"));
        };
        if parameters.len() != fields.len() {
            return Err(self.error(
                pattern.span.clone(),
                format!(
                    "constructor arity: expected {}, found {}",
                    parameters.len(),
                    fields.len()
                ),
            ));
        }
        self.unify(&result, expected, &pattern.span)?;
        Ok(Pattern::Constructor(
            canonical,
            fields
                .iter()
                .zip(parameters)
                .map(|(p, (_, t))| self.check_pattern(p, &t, environment, names))
                .collect::<Checked<_>>()?,
        ))
    }
    fn infer_match(
        &mut self,
        expr: &Expr,
        scrutinee: &Expr,
        arms: &[(ast::Pattern, Expr)],
        environment: &Environment,
    ) -> Checked<(Term, Type)> {
        let (scrutinee, scrutinee_type) = self.infer(scrutinee, &mut environment.clone())?;
        let result = self.fresh();
        let mut terms = Vec::new();
        let mut covered = false;
        for (pattern, body) in arms {
            if covered {
                return Err(self.error(
                    pattern.span.clone(),
                    "unreachable match arm after an exhaustive pattern",
                ));
            }
            let mut local = environment.clone();
            let mut names = BTreeSet::new();
            let pattern = self.check_pattern(pattern, &scrutinee_type, &mut local, &mut names)?;
            let (body, body_type) = self.infer(body, &mut local)?;
            self.unify(&body_type, &result, &expr.span)?;
            covered = matches!(pattern, Pattern::Bind(_) | Pattern::Wildcard);
            terms.push((pattern, body));
        }
        let ty = self.resolve_type(&scrutinee_type);
        if !self.patterns_exhaustive(
            &ty,
            &terms
                .iter()
                .map(|(p, _)| vec![p.clone()])
                .collect::<Vec<_>>(),
        ) {
            return Err(self.error(expr.span.clone(), format!("non-exhaustive match for {ty}")));
        }
        Ok((
            Term {
                ty: result.clone(),
                span: expr.span.clone(),
                kind: TermKind::Match(Box::new(scrutinee), terms),
            },
            result,
        ))
    }
    fn patterns_exhaustive(&self, ty: &Type, rows: &[Vec<Pattern>]) -> bool {
        self.exhaustive_matrix(std::slice::from_ref(ty), rows)
    }
    fn exhaustive_matrix(&self, types: &[Type], rows: &[Vec<Pattern>]) -> bool {
        if types.is_empty() {
            return !rows.is_empty();
        }
        if rows.iter().any(|row| {
            row.len() == types.len()
                && row
                    .iter()
                    .all(|p| matches!(p, Pattern::Wildcard | Pattern::Bind(_)))
        }) {
            return true;
        }
        if rows.is_empty() {
            return false;
        }
        let ty = self.resolve_type(&types[0]);
        let constructors = self.pattern_constructors(&ty);
        if constructors.is_empty() {
            let defaults = rows
                .iter()
                .filter(|r| matches!(r.first(), Some(Pattern::Wildcard | Pattern::Bind(_))))
                .map(|r| r[1..].to_vec())
                .collect::<Vec<_>>();
            return self.exhaustive_matrix(&types[1..], &defaults);
        }
        constructors.into_iter().all(|(tag, fields)| {
            let mut specialized = Vec::new();
            for row in rows {
                let Some(head) = row.first() else {
                    continue;
                };
                let mut prefix = match head {
                    Pattern::Wildcard | Pattern::Bind(_) => vec![Pattern::Wildcard; fields.len()],
                    Pattern::Constructor(n, p) if *n == tag => p.clone(),
                    Pattern::Literal(Literal::Bool(b)) if tag == b.to_string() => Vec::new(),
                    Pattern::Tuple(p) if tag == "$tuple" => p.clone(),
                    Pattern::Record(p) if tag == "$record" => {
                        if let Type::Record(types, _) = &ty {
                            types
                                .keys()
                                .map(|n| {
                                    p.iter()
                                        .find(|(m, _)| m == n)
                                        .map_or(Pattern::Wildcard, |(_, p)| p.clone())
                                })
                                .collect()
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                };
                prefix.extend_from_slice(&row[1..]);
                specialized.push(prefix);
            }
            let mut next = fields;
            next.extend_from_slice(&types[1..]);
            self.exhaustive_matrix(&next, &specialized)
        })
    }
    fn pattern_constructors(&self, ty: &Type) -> Vec<(String, Vec<Type>)> {
        match ty {
            Type::Bool => vec![("true".into(), vec![]), ("false".into(), vec![])],
            Type::Tuple(fields) => vec![(
                "$tuple".into(),
                fields.iter().map(|(_, t)| t.clone()).collect(),
            )],
            Type::Record(fields, _) => vec![("$record".into(), fields.values().cloned().collect())],
            Type::Nominal(n, args) if n == "Option" => vec![
                ("Option::Some".into(), args.clone()),
                ("Option::None".into(), vec![]),
            ],
            Type::Nominal(n, args) => self.enums.get(n).map_or_else(Vec::new, |e| {
                let parameters = e
                    .parameters
                    .iter()
                    .zip(args)
                    .map(|((n, _), t)| (n.clone(), t.clone()))
                    .collect();
                e.variants
                    .iter()
                    .map(|(n, fields)| {
                        (
                            n.clone(),
                            fields.iter().map(|t| t.substitute(&parameters)).collect(),
                        )
                    })
                    .collect()
            }),
            _ => Vec::new(),
        }
    }
    fn check_recursion(&self, name: &str, function: &Term) -> Checked<()> {
        let TermKind::Function(parameters, body) = &function.kind else {
            return Err(self.error(
                function.span.clone(),
                "recursive definition is not a function",
            ));
        };
        let provenance = parameters
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), (i, false)))
            .collect();
        self.structural_term(name, body, &provenance, parameters.len())
    }
    fn structural_term(
        &self,
        name: &str,
        term: &Term,
        provenance: &BTreeMap<String, (usize, bool)>,
        arity: usize,
    ) -> Checked<()> {
        match &term.kind {
            TermKind::Global(n, _, _) | TermKind::Local(n) if n == name => {
                return Err(Diagnostic::new(
                    &self.source,
                    term.span.clone(),
                    Phase::Termination,
                    "a recursive function cannot escape as a higher-order value",
                ));
            }
            TermKind::Call(function, args) if matches!(&function.kind,TermKind::Global(n,_,_)|TermKind::Local(n) if n==name) =>
            {
                let relations = args
                    .iter()
                    .map(|arg| {
                        if let TermKind::Local(n) = &arg.kind {
                            provenance.get(n).copied()
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                let decreasing = (0..arity).any(|i| {
                    relations.get(i) == Some(&Some((i, true)))
                        && (0..i).all(|j| relations.get(j) == Some(&Some((j, false))))
                });
                if !decreasing {
                    return Err(Diagnostic::new(
                        &self.source,
                        term.span.clone(),
                        Phase::Termination,
                        "recursive call does not decrease a structural argument lexicographically",
                    ));
                }
                for arg in args {
                    self.structural_term(name, arg, provenance, arity)?;
                }
                return Ok(());
            }
            TermKind::Match(scrutinee, arms) => {
                self.structural_term(name, scrutinee, provenance, arity)?;
                for (pattern, body) in arms {
                    let mut local = provenance.clone();
                    remove_pattern_names(pattern, &mut local);
                    if let TermKind::Local(n) = &scrutinee.kind
                        && let Some(origin) = provenance.get(n)
                    {
                        self.pattern_subterms(pattern, *origin, &mut local);
                    }
                    self.structural_term(name, body, &local, arity)?;
                }
                return Ok(());
            }
            TermKind::Let(pattern, value, body) => {
                self.structural_term(name, value, provenance, arity)?;
                let mut local = provenance.clone();
                remove_pattern_names(pattern, &mut local);
                if let TermKind::Local(n) = &value.kind
                    && let Some(origin) = provenance.get(n)
                {
                    self.pattern_subterms(pattern, *origin, &mut local);
                }
                return self.structural_term(name, body, &local, arity);
            }
            TermKind::Function(parameters, body) => {
                let mut local = provenance.clone();
                for p in parameters {
                    local.remove(p);
                }
                return self.structural_term(name, body, &local, arity);
            }
            TermKind::LetRec(local_name, parameters, body, tail) => {
                let mut body_provenance = provenance.clone();
                body_provenance.remove(local_name);
                for p in parameters {
                    body_provenance.remove(p);
                }
                self.structural_term(name, body, &body_provenance, arity)?;
                let mut tail_provenance = provenance.clone();
                tail_provenance.remove(local_name);
                return self.structural_term(name, tail, &tail_provenance, arity);
            }
            _ => {}
        }
        for child in term_children(term) {
            self.structural_term(name, child, provenance, arity)?;
        }
        Ok(())
    }
    fn pattern_subterms(
        &self,
        pattern: &Pattern,
        origin: (usize, bool),
        provenance: &mut BTreeMap<String, (usize, bool)>,
    ) {
        match pattern {
            Pattern::Bind(n) => {
                provenance.insert(n.clone(), origin);
            }
            Pattern::Constructor(name, patterns) => {
                if let Some((enum_name, _)) = name.rsplit_once("::")
                    && let Some(info) = self.enums.get(enum_name)
                    && let Some((_, fields)) = info.variants.iter().find(|(n, _)| n == name)
                {
                    for (pattern, field) in patterns.iter().zip(fields) {
                        if matches!(field,Type::Nominal(n,_) if n==enum_name) {
                            self.pattern_subterms(pattern, (origin.0, true), provenance);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
fn contains_nominal(ty: &Type, name: &str) -> bool {
    let mut found = false;
    ty.map(&mut |t| {
        if matches!(t,Type::Nominal(n,_) if n==name) {
            found = true;
        }
        None
    });
    found
}
fn has_parameter(ty: &Type) -> bool {
    let mut found = false;
    ty.map(&mut |t| {
        if matches!(t, Type::Parameter(_) | Type::Index(..)) {
            found = true;
        }
        None
    });
    found
}
fn describable(ty: &Type) -> bool {
    match ty {
        Type::Bool | Type::Integer | Type::Nat | Type::String | Type::Nominal(..) => true,
        Type::Function(args, result) => {
            args.iter().all(|(_, t)| describable(t)) && describable(result)
        }
        Type::Tuple(fields) => fields.iter().all(|(_, t)| describable(t)),
        Type::Record(fields, None) => fields.values().all(describable),
        Type::List(t) => describable(t),
        _ => false,
    }
}
fn remove_pattern_names(pattern: &Pattern, environment: &mut BTreeMap<String, (usize, bool)>) {
    match pattern {
        Pattern::Bind(n) => {
            environment.remove(n);
        }
        Pattern::Constructor(_, p) | Pattern::Tuple(p) => {
            for p in p {
                remove_pattern_names(p, environment);
            }
        }
        Pattern::Record(p) => {
            for (_, p) in p {
                remove_pattern_names(p, environment);
            }
        }
        _ => {}
    }
}
fn term_children(term: &Term) -> Vec<&Term> {
    match &term.kind {
        TermKind::Call(f, args) => std::iter::once(f.as_ref()).chain(args).collect(),
        TermKind::Coerce(t)
        | TermKind::Project(t, _)
        | TermKind::Unary(_, t)
        | TermKind::Named(_, t) => vec![t],
        TermKind::Index(a, b) | TermKind::Binary(_, a, b, _) | TermKind::Let(_, a, b) => vec![a, b],
        TermKind::Record(fields) => fields.iter().map(|(_, t)| t).collect(),
        TermKind::Tuple(fields) | TermKind::List(fields) => fields.iter().collect(),
        TermKind::If(a, b, c) => vec![a, b, c],
        TermKind::Function(_, t) => vec![t],
        TermKind::LetRec(_, _, body, tail) => vec![body, tail],
        TermKind::Match(t, arms) => std::iter::once(t.as_ref())
            .chain(arms.iter().map(|(_, t)| t))
            .collect(),
        _ => Vec::new(),
    }
}

fn reject_ambiguous(ty: &Type) -> Result<(), Type> {
    let mut rows = BTreeSet::new();
    ty.map(&mut |t| {
        if let Type::Record(_, Some(tail)) = t {
            rows.extend(tail.variables());
        }
        None
    });
    if let Some(variable) = ty.variables().difference(&rows).next() {
        Err(Type::Variable(*variable))
    } else {
        Ok(())
    }
}

fn reject_unsolved_arguments(term: &Term) -> Result<(), Span> {
    if let TermKind::Global(_, args, _) = &term.kind
        && args.values().any(|ty| !ty.variables().is_empty())
    {
        return Err(term.span.clone());
    }
    for child in term_children(term) {
        reject_unsolved_arguments(child)?;
    }
    Ok(())
}
