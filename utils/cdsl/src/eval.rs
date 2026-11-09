//! Pure evaluation of checked modules.
use crate::ast::{Literal, Span};
use crate::core::{Definition, Pattern, Term, TermKind};
use crate::diagnostic::{Diagnostic, Phase};
use crate::types::Type;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

type Environment = BTreeMap<String, Value>;
type Evaluation = Result<Value, Diagnostic>;

/// Resource bounds for one evaluation, independent of termination checking.
#[derive(Clone, Copy, Debug)]
pub struct EvaluationLimits {
    /// Maximum number of nested evaluation frames.
    pub max_depth: usize,
    /// Maximum number of core terms evaluated.
    pub max_steps: usize,
}

impl Default for EvaluationLimits {
    fn default() -> Self {
        Self {
            max_depth: 512,
            max_steps: 100_000,
        }
    }
}

/// Immutable lexical function value.
#[derive(Clone, Debug)]
pub struct Closure {
    parameters: Vec<String>,
    body: Term,
    environment: Environment,
    types: BTreeMap<String, Type>,
    source: String,
    recursive: Option<String>,
}
impl Closure {
    /// Inspect the checked body for compiler output.
    pub fn code(&self) -> &Term {
        &self.body
    }
    /// Inspect the flattened function parameter names in call order.
    pub fn parameters(&self) -> &[String] {
        &self.parameters
    }
    /// Inspect immutable values captured by the checked function.
    pub fn captures(&self) -> &BTreeMap<String, Value> {
        &self.environment
    }
    /// Inspect the checked type and static arguments retained by the function.
    pub fn type_arguments(&self) -> &BTreeMap<String, Type> {
        &self.types
    }
}
/// A pure value produced by a checked source module.
#[derive(Clone, Debug)]
pub enum Value {
    /// Semantic Boolean.
    Bool(bool),
    /// Checked signed 128-bit integer, also used for nonnegative `Nat`.
    Integer(i128),
    /// String.
    String(String),
    /// Ordered immutable record.
    Record(Vec<(String, Value)>),
    /// Product.
    Tuple(Vec<Value>),
    /// Homogeneous immutable list.
    List(Vec<Value>),
    /// Nominal constructor with immutable fields.
    Constructor(String, Vec<Value>),
    /// Typed description, preserving function labels and product shape.
    Description(Type),
    /// Qualified definition identity paired with its payload.
    Named {
        identity: String,
        payload: Box<Value>,
    },
    /// Typed host reference. Only named trusted library bindings execute here.
    Foreign {
        /// Stable package-qualified declaration identity.
        identity: String,
        /// Specialized decoded host binding type.
        ty: Type,
        /// Explicit generic substitution for the host binding.
        arguments: BTreeMap<String, Type>,
    },
    /// Immutable function closure.
    Function(Rc<Closure>),
    /// Partially applied constructor, accepting one complete flattened call.
    ConstructorFunction(String, usize),
    /// Opaque typed standard-library description.
    Relational(Rc<crate::relations::RelationalValue>),
}
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Integer(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s:?}"),
            Self::Record(fields) => write!(
                f,
                "{{{}}}",
                fields
                    .iter()
                    .map(|(n, v)| format!("{n}: {v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Tuple(fields) => write!(
                f,
                "({}{})",
                display_values(fields),
                if fields.len() == 1 { "," } else { "" }
            ),
            Self::List(fields) => write!(f, "[{}]", display_values(fields)),
            Self::Constructor(name, fields) => write!(f, "{name}({})", display_values(fields)),
            Self::Description(ty) => write!(f, "type({ty})"),
            Self::Named { identity, payload } => write!(f, "Named({identity:?}, {payload})"),
            Self::Foreign { identity, .. } => write!(f, "<foreign {identity}>"),
            Self::Function(_) => f.write_str("<function>"),
            Self::ConstructorFunction(n, _) => write!(f, "<constructor {n}>"),
            Self::Relational(_) => f.write_str("<relational description>"),
        }
    }
}
fn display_values(values: &[Value]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) struct Evaluator<'a> {
    definitions: &'a BTreeMap<String, Definition>,
    depth: usize,
    limits: EvaluationLimits,
    steps_remaining: usize,
    next_symbolic_scope: u64,
}
impl<'a> Evaluator<'a> {
    pub fn new(definitions: &'a BTreeMap<String, Definition>) -> Self {
        Self::with_limits(definitions, EvaluationLimits::default())
    }

    pub fn with_limits(
        definitions: &'a BTreeMap<String, Definition>,
        limits: EvaluationLimits,
    ) -> Self {
        Self {
            definitions,
            depth: 0,
            limits,
            steps_remaining: limits.max_steps,
            next_symbolic_scope: 0,
        }
    }
    pub fn global(
        &mut self,
        name: &str,
        types: &BTreeMap<String, Type>,
        rows: &BTreeMap<u32, Type>,
    ) -> Evaluation {
        let definition = self.definitions.get(name).ok_or_else(|| {
            Diagnostic::new(
                "",
                0..0,
                Phase::Evaluate,
                format!("unknown checked binding `{name}`"),
            )
        })?;
        if definition
            .scheme
            .parameters
            .iter()
            .any(|(n, _)| !types.contains_key(n))
        {
            return Err(Diagnostic::new(
                &definition.source,
                definition.term.span.clone(),
                Phase::Evaluate,
                format!("generic binding `{name}` requires explicit specialization in source"),
            ));
        }
        let environment = types
            .iter()
            .filter_map(|(n, t)| match t {
                Type::IntegerIndex(v) => Some((n.clone(), Value::Integer(*v))),
                Type::BooleanIndex(v) => Some((n.clone(), Value::Bool(*v))),
                Type::StringIndex(v) => Some((n.clone(), Value::String(v.clone()))),
                _ => None,
            })
            .collect();
        if rows.is_empty() {
            self.term(&definition.source, &definition.term, &environment, types)
        } else {
            let mut term = definition.term.clone();
            term.map_types(&mut |ty| ty.substitute_variables(rows));
            self.term(&definition.source, &term, &environment, types)
        }
    }
    pub fn expression(&mut self, source: &str, term: &Term) -> Evaluation {
        self.term(source, term, &BTreeMap::new(), &BTreeMap::new())
    }
    fn error(source: &str, span: &Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(source, span.clone(), Phase::Evaluate, message)
    }
    fn term(
        &mut self,
        source: &str,
        term: &Term,
        environment: &Environment,
        types: &BTreeMap<String, Type>,
    ) -> Evaluation {
        if self.depth >= self.limits.max_depth {
            return Err(Self::error(
                source,
                &term.span,
                "evaluation nesting exceeds the resource limit; this limit is independent of structural termination checking",
            ));
        }
        if self.steps_remaining == 0 {
            return Err(Self::error(
                source,
                &term.span,
                "evaluation exceeds the step budget; this resource limit is independent of structural termination checking",
            ));
        }
        self.steps_remaining -= 1;
        self.depth += 1;
        let result = self.term_inner(source, term, environment, types);
        self.depth -= 1;
        result
    }
    fn term_inner(
        &mut self,
        source: &str,
        term: &Term,
        environment: &Environment,
        types: &BTreeMap<String, Type>,
    ) -> Evaluation {
        match &term.kind {
            TermKind::Literal(literal) => Ok(literal_value(literal)),
            TermKind::Local(name) => environment.get(name).cloned().ok_or_else(|| {
                let message = if matches!(types.get(name), Some(Type::Index(..))) {
                    format!(
                        "static parameter `{name}` requires specialization to evaluate its value"
                    )
                } else {
                    format!("missing checked local `{name}`")
                };
                Self::error(source, &term.span, message)
            }),
            TermKind::Global(name, arguments, rows) => {
                let arguments = arguments
                    .iter()
                    .map(|(n, t)| (n.clone(), t.substitute(types)))
                    .collect();
                let rows = rows
                    .iter()
                    .map(|(variable, ty)| (*variable, ty.substitute(types)))
                    .collect();
                self.global(name, &arguments, &rows)
            }
            TermKind::Description(ty) => Ok(Value::Description(ty.substitute(types))),
            TermKind::Function(parameters, body) => Ok(Value::Function(Rc::new(Closure {
                parameters: parameters.clone(),
                body: body.as_ref().clone(),
                environment: environment.clone(),
                types: types.clone(),
                source: source.into(),
                recursive: None,
            }))),
            TermKind::Call(function, args) => {
                let function = self.term(source, function, environment, types)?;
                let args = args
                    .iter()
                    .map(|a| self.term(source, a, environment, types))
                    .collect::<Result<Vec<_>, _>>()?;
                self.call(source, &term.span, function, args)
            }
            TermKind::Project(value, field) => {
                let value = self.term(source, value, environment, types)?;
                project(value, field).ok_or_else(|| {
                    Self::error(source, &term.span, "checked projection invariant failed")
                })
            }
            TermKind::Index(value, index) => {
                let value = self.term(source, value, environment, types)?;
                let index = self.term(source, index, environment, types)?;
                if let (Value::List(values), Value::Integer(index)) = (value, index) {
                    Ok(usize::try_from(index)
                        .ok()
                        .and_then(|i| values.get(i))
                        .cloned()
                        .map_or_else(
                            || Value::Constructor("Option::None".into(), Vec::new()),
                            |v| Value::Constructor("Option::Some".into(), vec![v]),
                        ))
                } else {
                    Err(Self::error(
                        source,
                        &term.span,
                        "checked indexing invariant failed",
                    ))
                }
            }
            TermKind::Record(fields) => Ok(Value::Record(
                fields
                    .iter()
                    .map(|(n, v)| Ok((n.clone(), self.term(source, v, environment, types)?)))
                    .collect::<Result<_, Diagnostic>>()?,
            )),
            TermKind::Tuple(values) => Ok(Value::Tuple(
                values
                    .iter()
                    .map(|v| self.term(source, v, environment, types))
                    .collect::<Result<_, _>>()?,
            )),
            TermKind::List(values) => Ok(Value::List(
                values
                    .iter()
                    .map(|v| self.term(source, v, environment, types))
                    .collect::<Result<_, _>>()?,
            )),
            TermKind::Let(pattern, value, body) => {
                let value = self.term(source, value, environment, types)?;
                let mut local = environment.clone();
                if !bind_pattern(pattern, &value, &mut local) {
                    return Err(Self::error(
                        source,
                        &term.span,
                        "checked exhaustive binding invariant failed",
                    ));
                }
                self.term(source, body, &local, types)
            }
            TermKind::LetRec(name, parameters, body, tail) => {
                let function = Value::Function(Rc::new(Closure {
                    parameters: parameters.clone(),
                    body: body.as_ref().clone(),
                    environment: environment.clone(),
                    types: types.clone(),
                    source: source.into(),
                    recursive: Some(name.clone()),
                }));
                let mut local = environment.clone();
                local.insert(name.clone(), function);
                self.term(source, tail, &local, types)
            }
            TermKind::If(condition, yes, no) => {
                match self.term(source, condition, environment, types)? {
                    Value::Bool(true) => self.term(source, yes, environment, types),
                    Value::Bool(false) => self.term(source, no, environment, types),
                    _ => Err(Self::error(
                        source,
                        &term.span,
                        "checked condition invariant failed",
                    )),
                }
            }
            TermKind::Match(scrutinee, arms) => {
                let value = self.term(source, scrutinee, environment, types)?;
                for (pattern, body) in arms {
                    let mut local = environment.clone();
                    if bind_pattern(pattern, &value, &mut local) {
                        return self.term(source, body, &local, types);
                    }
                }
                Err(Self::error(
                    source,
                    &term.span,
                    "checked exhaustiveness invariant failed",
                ))
            }
            TermKind::Unary(operator, value) => {
                let value = self.term(source, value, environment, types)?;
                unary(operator, value).ok_or_else(|| {
                    Self::error(
                        source,
                        &term.span,
                        "integer negation overflow or unavailable unary dictionary",
                    )
                })
            }
            TermKind::Binary(operator, left, right, ty) => self.binary(
                source,
                &term.span,
                operator,
                (left, right, ty),
                environment,
                types,
            ),
            TermKind::Constructor(name, arity) => {
                Ok(Value::ConstructorFunction(name.clone(), *arity))
            }
            TermKind::Foreign(identity) => Ok(Value::Foreign {
                identity: identity.clone(),
                ty: term.ty.substitute(types),
                arguments: types.clone(),
            }),
            TermKind::Trusted(name) => Ok(crate::relations::trusted(name, types)),
            TermKind::Coerce(value) => {
                let value = self.term(source, value, environment, types)?;
                crate::relations::coerce_value(value, &term.ty.substitute(types))
                    .map_err(|message| Self::error(source, &term.span, message))
            }
            TermKind::Named(identity, payload) => Ok(Value::Named {
                identity: format!(
                    "{}{}",
                    identity,
                    if types.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "<{}>",
                            types
                                .iter()
                                .map(|(n, t)| format!("{n}={t}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                ),
                payload: Box::new(self.term(source, payload, environment, types)?),
            }),
        }
    }
    pub(crate) fn fresh_symbolic_scope(&mut self) -> u64 {
        let scope = self.next_symbolic_scope;
        self.next_symbolic_scope += 1;
        scope
    }
    pub(crate) fn call(
        &mut self,
        source: &str,
        span: &Span,
        function: Value,
        args: Vec<Value>,
    ) -> Evaluation {
        if let Some((name, types)) = crate::relations::host_binding(&function) {
            return crate::relations::host_call(self, name, types, &args, source, span);
        }
        if let Some(result) = crate::relations::call_value(&function, &args) {
            return result.map_err(|message| Self::error(source, span, message));
        }
        match function {
            Value::Function(function) => {
                if function.parameters.len() != args.len() {
                    return Err(Self::error(
                        source,
                        span,
                        "checked function arity invariant failed",
                    ));
                }
                let mut environment = function.environment.clone();
                if let Some(name) = &function.recursive {
                    environment.insert(name.clone(), Value::Function(function.clone()));
                }
                environment.extend(function.parameters.iter().cloned().zip(args));
                self.term(
                    &function.source,
                    &function.body,
                    &environment,
                    &function.types,
                )
            }
            Value::ConstructorFunction(name, arity) if args.len() == arity => {
                Ok(Value::Constructor(name, args))
            }
            Value::Foreign { identity, .. } => Err(Self::error(
                source,
                span,
                format!("opaque foreign binding `{identity}` has no evaluator binding"),
            )),
            _ => Err(Self::error(
                source,
                span,
                "checked callable invariant failed",
            )),
        }
    }
    fn binary(
        &mut self,
        source: &str,
        span: &Span,
        operator: &str,
        operands: (&Term, &Term, &Type),
        environment: &Environment,
        types: &BTreeMap<String, Type>,
    ) -> Evaluation {
        let (left, right, ty) = operands;
        let left = self.term(source, left, environment, types)?;
        if operator == "&&" && matches!(left, Value::Bool(false)) {
            return Ok(left);
        }
        if operator == "||" && matches!(left, Value::Bool(true)) {
            return Ok(left);
        }
        let right = self.term(source, right, environment, types)?;
        if let Some(result) = crate::relations::binary(operator, &left, &right) {
            return result.map_err(|message| Self::error(source, span, message));
        }
        let result = scalar_binary(operator, left, right);
        if ty.substitute(types) == Type::Nat && matches!(result,Some(Value::Integer(n)) if n<0) {
            return Err(Self::error(
                source,
                span,
                "Nat arithmetic cannot produce a negative value",
            ));
        }
        result.ok_or_else(||Self::error(source,span,"integer arithmetic overflow, division by zero, invalid shift, or unavailable scalar dictionary"))
    }
}
fn literal_value(literal: &Literal) -> Value {
    match literal {
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Integer(n) => Value::Integer(*n),
        Literal::String(s) => Value::String(s.clone()),
    }
}
fn project(value: Value, field: &str) -> Option<Value> {
    if let Some(projected) = crate::relations::project(&value, field) {
        return Some(projected);
    }
    match value {
        Value::Record(fields) => fields.into_iter().find(|(n, _)| n == field).map(|(_, v)| v),
        Value::Tuple(fields) => field
            .parse::<usize>()
            .ok()
            .and_then(|i| fields.into_iter().nth(i)),
        Value::Named { identity, .. } if field == "identity" => Some(Value::String(identity)),
        Value::Named { payload, .. } if field == "payload" => Some(*payload),
        _ => None,
    }
}
fn bind_pattern(pattern: &Pattern, value: &Value, environment: &mut Environment) -> bool {
    match (pattern, value) {
        (Pattern::Wildcard, _) => true,
        (Pattern::Bind(n), v) => {
            environment.insert(n.clone(), v.clone());
            true
        }
        (Pattern::Literal(a), b) => match (a, b) {
            (Literal::Bool(a), Value::Bool(b)) => a == b,
            (Literal::Integer(a), Value::Integer(b)) => a == b,
            (Literal::String(a), Value::String(b)) => a == b,
            _ => false,
        },
        (Pattern::Constructor(a, patterns), Value::Constructor(b, values)) if a == b => {
            bind_fields(patterns, values, environment)
        }
        (Pattern::Tuple(patterns), Value::Tuple(values)) => {
            bind_fields(patterns, values, environment)
        }
        (Pattern::Record(patterns), Value::Record(values)) => patterns.iter().all(|(n, p)| {
            values
                .iter()
                .find(|(m, _)| m == n)
                .is_some_and(|(_, v)| bind_pattern(p, v, environment))
        }),
        _ => false,
    }
}
fn bind_fields(patterns: &[Pattern], values: &[Value], environment: &mut Environment) -> bool {
    patterns.len() == values.len()
        && patterns
            .iter()
            .zip(values)
            .all(|(p, v)| bind_pattern(p, v, environment))
}
fn unary(operator: &str, value: Value) -> Option<Value> {
    match (operator, value) {
        ("!", Value::Bool(b)) => Some(Value::Bool(!b)),
        ("-", Value::Integer(n)) => n.checked_neg().map(Value::Integer),
        _ => None,
    }
}
fn scalar_binary(operator: &str, left: Value, right: Value) -> Option<Value> {
    match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => integer_binary(operator, a, b),
        (Value::Bool(a), Value::Bool(b)) => Some(Value::Bool(match operator {
            "&&" | "&" => a && b,
            "||" | "|" => a || b,
            "^" => a ^ b,
            "==" => a == b,
            "!=" => a != b,
            _ => return None,
        })),
        (Value::String(a), Value::String(b)) => Some(Value::Bool(match operator {
            "==" => a == b,
            "!=" => a != b,
            "<" => a < b,
            "<=" => a <= b,
            ">" => a > b,
            ">=" => a >= b,
            _ => return None,
        })),
        _ => None,
    }
}
fn integer_binary(operator: &str, a: i128, b: i128) -> Option<Value> {
    let numeric = match operator {
        "+" => a.checked_add(b),
        "-" => a.checked_sub(b),
        "*" => a.checked_mul(b),
        "/" => a.checked_div(b),
        "%" => a.checked_rem(b),
        "<<" => u32::try_from(b).ok().and_then(|b| a.checked_shl(b)),
        ">>" => u32::try_from(b).ok().and_then(|b| a.checked_shr(b)),
        "&" => Some(a & b),
        "|" => Some(a | b),
        "^" => Some(a ^ b),
        "==" => return Some(Value::Bool(a == b)),
        "!=" => return Some(Value::Bool(a != b)),
        "<" => return Some(Value::Bool(a < b)),
        "<=" => return Some(Value::Bool(a <= b)),
        ">" => return Some(Value::Bool(a > b)),
        ">=" => return Some(Value::Bool(a >= b)),
        _ => None,
    };
    numeric.map(Value::Integer)
}
