//! Typed relational descriptions and their Egglog text backend.
use crate::ast::Span;
use crate::compiler::{CheckedModule, SourceMap};
use crate::diagnostic::{Diagnostic, Phase};
use crate::eval::{Evaluator, Value};
use crate::types::Type;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

pub(crate) const SOURCE: &str = include_str!("relations.cdsl");

/// Supply the standard relational library for `mod relational;`.
pub fn install(sources: &mut SourceMap) {
    sources.insert("relational", SOURCE);
}

fn nominal(name: &str, arguments: Vec<Type>) -> Type {
    Type::Nominal(format!("relational::{name}"), arguments)
}
fn arguments<'a>(ty: &'a Type, name: &str) -> Option<&'a [Type]> {
    match ty {
        Type::Nominal(identity, arguments) if identity == &format!("relational::{name}") => {
            Some(arguments)
        }
        _ => None,
    }
}
pub(crate) fn expression_sort(ty: &Type) -> Option<&Type> {
    match arguments(ty, "Expr")? {
        [sort] => Some(sort),
        _ => None,
    }
}
fn fields(ty: &Type) -> Vec<Type> {
    match ty {
        Type::Tuple(fields) => fields.iter().map(|(_, ty)| ty.clone()).collect(),
        _ => vec![ty.clone()],
    }
}
pub(crate) fn call_type(ty: &Type) -> Option<Type> {
    let (schema, result) =
        if let Some([schema]) = arguments(ty, "Relation").or_else(|| arguments(ty, "Predicate")) {
            (fields(schema), nominal("Goal", vec![]))
        } else if let Some([Type::Function(parameters, result)]) = arguments(ty, "Constructor") {
            (
                parameters.iter().map(|(_, ty)| ty.clone()).collect(),
                nominal("Expr", vec![result.as_ref().clone()]),
            )
        } else {
            return None;
        };
    Some(Type::Function(
        schema
            .into_iter()
            .enumerate()
            .map(|(i, sort)| (format!("_{i}"), nominal("Expr", vec![sort])))
            .collect(),
        Box::new(result),
    ))
}
pub(crate) fn projection_type(ty: &Type, field: &str) -> Option<Type> {
    let [key, value] = arguments(ty, "Table")? else {
        return None;
    };
    let mut schema = fields(key);
    if field == "remove" {
        return Some(nominal(
            "Relation",
            vec![Type::Tuple(
                schema.into_iter().map(|ty| (None, ty)).collect(),
            )],
        ));
    }
    if field == "get" {
        return Some(nominal(
            "Constructor",
            vec![Type::Function(
                schema
                    .into_iter()
                    .enumerate()
                    .map(|(index, ty)| (format!("_{index}"), ty))
                    .collect(),
                Box::new(value.clone()),
            )],
        ));
    }
    if field != "has" {
        return None;
    }
    schema.push(value.clone());
    Some(nominal(
        "Relation",
        vec![Type::Tuple(
            schema.into_iter().map(|ty| (None, ty)).collect(),
        )],
    ))
}
pub(crate) fn coerces(source: &Type, target: &Type) -> bool {
    arguments(target, "Declaration").is_some()
        && ["Relation", "Table"]
            .iter()
            .any(|name| arguments(source, name).is_some())
}
pub(crate) fn common_type(left: &Type, right: &Type) -> Option<Type> {
    let declaration = |ty: &Type| {
        ["Relation", "Table", "Declaration"]
            .iter()
            .any(|name| arguments(ty, name).is_some())
    };
    (left != right && declaration(left) && declaration(right))
        .then(|| nominal("Declaration", vec![]))
}
pub(crate) fn binary_type(operator: &str, left: &Type) -> Option<(Type, Type)> {
    if arguments(left, "Goal").is_some() && operator == "&" {
        return Some((left.clone(), left.clone()));
    }
    let sort = expression_sort(left)?;
    let numeric = matches!(sort, Type::Nat | Type::Integer)
        || matches!(sort, Type::Nominal(name, args) if args.is_empty() && name.rsplit("::").next() == Some("BigInt"));
    match operator {
        "==" | "!=" => Some((left.clone(), nominal("Goal", vec![]))),
        "<" | ">" | "<=" | ">=" if numeric || *sort == Type::String => {
            Some((left.clone(), nominal("Goal", vec![])))
        }
        "+" if numeric || *sort == Type::String => Some((left.clone(), left.clone())),
        "-" | "*" | "/" | "%" | "<<" | ">>" | "&" | "|" if numeric => {
            Some((left.clone(), left.clone()))
        }
        _ => None,
    }
}

/// Opaque evaluated library value. Its representation is private to the backend.
#[derive(Clone, Debug)]
pub struct RelationalValue(Kind);
#[derive(Clone, Debug)]
enum Kind {
    Host(String, BTreeMap<String, Type>),
    Expr(Expression),
    Goal(Vec<Fact>),
    Relation {
        key: String,
        schema: Vec<Type>,
    },
    Table {
        key: String,
        keys: Vec<Type>,
        value: Type,
        merge: Option<Expression>,
    },
    Constructor {
        name: String,
    },
    Predicate {
        name: String,
    },
    Removal {
        name: String,
        schema: Schema,
    },
    Merge(Type, Expression),
    Container {
        name: String,
        kind: String,
        arguments: Vec<Type>,
    },
    Clause {
        body: Vec<Fact>,
        head: Vec<Fact>,
    },
    Stage(Vec<RelationalValue>),
    Result(Box<RelationalValue>),
    Program {
        tables: Vec<RelationalValue>,
        stages: Vec<RelationalValue>,
        result: Box<RelationalValue>,
    },
    Marker(String),
}
#[derive(Clone, Debug)]
enum Expression {
    Variable(u64, usize),
    MergeOld,
    MergeNew,
    Call(String, Vec<Expression>),
    Integer(i128),
    String(String),
    Bool(bool),
}
#[derive(Clone, Debug)]
enum Fact {
    Atom(String, Vec<Expression>, Schema),
    Erase(String, Vec<Expression>, Schema),
    Predicate(String, Vec<Expression>),
    Equal(Expression, Expression),
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Schema {
    Relation(Vec<Type>),
    Table(Vec<Type>, Type),
}
fn declaration(value: &RelationalValue) -> Result<(&str, Schema), String> {
    match &value.0 {
        Kind::Relation { key, schema } => Ok((key, Schema::Relation(schema.clone()))),
        Kind::Table {
            key, keys, value, ..
        } => Ok((key, Schema::Table(keys.clone(), value.clone()))),
        _ => Err("expected a relation or table declaration".into()),
    }
}
fn value(kind: Kind) -> Value {
    Value::Relational(Rc::new(RelationalValue(kind)))
}
fn error(source: &str, span: &Span, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(source, span.clone(), Phase::Evaluate, message)
}
fn native(value: &Value) -> Result<&RelationalValue, String> {
    if let Value::Relational(value) = value {
        Ok(value)
    } else {
        Err("expected a typed relational description".into())
    }
}
fn expression(value: &Value) -> Result<Expression, String> {
    match value {
        Value::Relational(value) => match &value.0 {
            Kind::Expr(expression) => Ok(expression.clone()),
            _ => Err("expected Expr<T>".into()),
        },
        Value::Integer(value) => Ok(Expression::Integer(*value)),
        Value::String(value) => Ok(Expression::String(value.clone())),
        Value::Bool(value) => Ok(Expression::Bool(*value)),
        _ => Err("unsupported engine expression".into()),
    }
}
fn goal(value: &Value) -> Result<Vec<Fact>, String> {
    match &native(value)?.0 {
        Kind::Goal(facts) => Ok(facts.clone()),
        _ => Err("expected a symbolic goal".into()),
    }
}
fn string(value: &Value) -> Result<String, String> {
    if let Value::String(value) = value {
        Ok(value.clone())
    } else {
        Err("expected a string".into())
    }
}
fn collection(value: &Value) -> Result<&[Value], String> {
    match value {
        Value::List(values) | Value::Tuple(values) => Ok(values),
        _ => Err("expected a list or tuple".into()),
    }
}
fn record_field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, String> {
    if let Value::Record(fields) = value {
        fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
            .ok_or_else(|| format!("missing `{name}`"))
    } else {
        Err("expected a record".into())
    }
}
pub(crate) fn trusted(name: &str, types: &BTreeMap<String, Type>) -> Value {
    match name {
        "truth" => value(Kind::Goal(Vec::new())),
        "unique" | "fixed_point" | "analysis_only" => value(Kind::Marker(name.into())),
        _ => value(Kind::Host(name.into(), types.clone())),
    }
}
pub(crate) fn host_binding(value: &Value) -> Option<(&str, &BTreeMap<String, Type>)> {
    let Kind::Host(name, types) = &native(value).ok()?.0 else {
        return None;
    };
    Some((name, types))
}
pub(crate) fn coerce_value(original: Value, target: &Type) -> Result<Value, String> {
    if expression_sort(target).is_some() {
        return expression(&original).map(|expression| value(Kind::Expr(expression)));
    }
    match (original, target) {
        (Value::List(values), Type::List(target)) => values
            .into_iter()
            .map(|value| coerce_value(value, target))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        (Value::Record(fields), Type::Record(types, None)) => fields
            .into_iter()
            .map(|(name, value)| {
                let value = coerce_value(value, &types[&name])?;
                Ok((name, value))
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Value::Record),
        (value, _) => Ok(value),
    }
}
pub(crate) fn project(value: &Value, field: &str) -> Option<Value> {
    let Kind::Table { key, .. } = &native(value).ok()?.0 else {
        return None;
    };
    match field {
        "has" => Some(value.clone()),
        "get" => Some(self::value(Kind::Constructor { name: key.clone() })),
        "remove" => Some(self::value(Kind::Removal {
            name: key.clone(),
            schema: declaration(native(value).ok()?).ok()?.1,
        })),
        _ => None,
    }
}
pub(crate) fn binary(operator: &str, left: &Value, right: &Value) -> Option<Result<Value, String>> {
    if !matches!(left, Value::Relational(_)) && !matches!(right, Value::Relational(_)) {
        return None;
    }
    Some(match operator {
        "&" if matches!(native(left).map(|value| &value.0), Ok(Kind::Goal(_))) => goal(left)
            .and_then(|mut left| {
                left.extend(goal(right)?);
                Ok(value(Kind::Goal(left)))
            }),
        "==" => expression(left).and_then(|left| {
            Ok(value(Kind::Goal(vec![Fact::Equal(
                left,
                expression(right)?,
            )])))
        }),
        "!=" | "<" | ">" | "<=" | ">=" => expression(left).and_then(|left| {
            Ok(value(Kind::Goal(vec![Fact::Predicate(
                operator.into(),
                vec![left, expression(right)?],
            )])))
        }),
        "+" | "-" | "*" | "/" | "%" | "<<" | ">>" | "&" | "|" => {
            expression(left).and_then(|left| {
                Ok(value(Kind::Expr(Expression::Call(
                    operator.into(),
                    vec![left, expression(right)?],
                ))))
            })
        }
        _ => Err(format!("unsupported symbolic operator `{operator}`")),
    })
}
pub(crate) fn call_value(function: &Value, args: &[Value]) -> Option<Result<Value, String>> {
    let library = native(function).ok()?;
    Some(match &library.0 {
        Kind::Relation { key, .. } | Kind::Table { key, .. } => args
            .iter()
            .map(expression)
            .collect::<Result<Vec<_>, _>>()
            .and_then(|args| {
                let (_, schema) = declaration(library)?;
                Ok(value(Kind::Goal(vec![Fact::Atom(
                    key.clone(),
                    args,
                    schema,
                )])))
            }),
        Kind::Constructor { name, .. } => args
            .iter()
            .map(expression)
            .collect::<Result<Vec<_>, _>>()
            .map(|args| value(Kind::Expr(Expression::Call(name.clone(), args)))),
        Kind::Predicate { name } => args
            .iter()
            .map(expression)
            .collect::<Result<Vec<_>, _>>()
            .map(|args| value(Kind::Goal(vec![Fact::Predicate(name.clone(), args)]))),
        Kind::Removal { name, schema } => args
            .iter()
            .map(expression)
            .collect::<Result<Vec<_>, _>>()
            .map(|args| {
                value(Kind::Goal(vec![Fact::Erase(
                    name.clone(),
                    args,
                    schema.clone(),
                )]))
            }),
        _ => Err("relational value is not callable".into()),
    })
}
pub(crate) fn host_call(
    evaluator: &mut Evaluator<'_>,
    name: &str,
    types: &BTreeMap<String, Type>,
    args: &[Value],
    source: &str,
    span: &Span,
) -> Result<Value, Diagnostic> {
    host_call_inner(evaluator, name, types, args, source, span)
        .map_err(|message| error(source, span, message))
}
fn host_call_inner(
    evaluator: &mut Evaluator<'_>,
    name: &str,
    types: &BTreeMap<String, Type>,
    args: &[Value],
    source: &str,
    span: &Span,
) -> Result<Value, String> {
    let parameter = |name: &str| {
        types
            .get(name)
            .cloned()
            .ok_or_else(|| format!("missing static parameter `{name}`"))
    };
    match (name, args) {
        ("relation", [key]) => Ok(value(Kind::Relation {
            key: string(key)?,
            schema: fields(&parameter("S")?),
        })),
        ("table", [key, policy]) => {
            let table_value = parameter("V")?;
            let merge = match &native(policy)?.0 {
                Kind::Marker(policy) if policy == "unique" => None,
                Kind::Merge(sort, expression) if *sort == table_value => Some(expression.clone()),
                Kind::Merge(..) => {
                    return Err("table merge policy does not match its value sort".into());
                }
                _ => return Err("unsupported table merge policy".into()),
            };
            Ok(value(Kind::Table {
                key: string(key)?,
                keys: fields(&parameter("K")?),
                value: table_value,
                merge,
            }))
        }
        ("primitive", [name]) => {
            let Type::Function(parameters, result) = parameter("F")? else {
                return Err("primitive requires a function type".into());
            };
            for ty in parameters
                .iter()
                .map(|(_, ty)| ty)
                .chain(std::iter::once(result.as_ref()))
            {
                sort(ty)?;
            }
            Ok(value(Kind::Constructor {
                name: string(name)?,
            }))
        }
        ("predicate", [name]) => {
            for ty in fields(&parameter("S")?) {
                sort(&ty)?;
            }
            Ok(value(Kind::Predicate {
                name: string(name)?,
            }))
        }
        ("merge", [body]) => {
            let Type::Function(parameters, result) = parameter("F")? else {
                return Err("merge requires a function".into());
            };
            let [(_, old), (_, new)] = parameters.as_slice() else {
                return Err("merge requires exactly two Expr<T> parameters".into());
            };
            let Some(merge_sort) = expression_sort(old) else {
                return Err("merge requires Expr<T> parameters".into());
            };
            if old != new || result.as_ref() != old {
                return Err("merge parameters and result must have the same Expr<T> type".into());
            }
            sort(merge_sort)?;
            let completed = evaluator
                .call(
                    source,
                    span,
                    body.clone(),
                    vec![
                        value(Kind::Expr(Expression::MergeOld)),
                        value(Kind::Expr(Expression::MergeNew)),
                    ],
                )
                .map_err(|error| error.to_string())?;
            let expression = expression(&completed)?;
            if !variables(&[Fact::Equal(expression.clone(), expression.clone())]).is_empty() {
                return Err("a quantified variable escaped into a merge policy".into());
            }
            Ok(value(Kind::Merge(merge_sort.clone(), expression)))
        }
        ("erase", [row]) => {
            let facts = goal(row)?
                .into_iter()
                .map(|fact| match fact {
                    Fact::Atom(name, mut args, schema) => {
                        if let Schema::Table(keys, _) = &schema {
                            args.truncate(keys.len());
                        }
                        Ok(Fact::Erase(name, args, schema))
                    }
                    _ => Err("erase requires positive relation or table rows".into()),
                })
                .collect::<Result<Vec<_>, String>>()?;
            if facts.is_empty() {
                return Err("erase requires a positive relation or table row".into());
            }
            Ok(value(Kind::Goal(facts)))
        }
        ("container", []) => {
            let schema = parameter("S")?;
            if !matches!(&schema, Type::Nominal(_, args) if args.is_empty()) {
                return Err("a container sort must be a nominal type without arguments".into());
            }
            let container = parameter("C")?;
            let (kind, arguments) = [("Set", 1), ("Vec", 1), ("Map", 2)]
                .into_iter()
                .find_map(|(name, arity)| {
                    arguments(&container, name)
                        .filter(|args| args.len() == arity)
                        .map(|args| (name, args))
                })
                .ok_or_else(|| {
                    "container requires relational::Set<T>, Vec<T>, or Map<K,V>".to_string()
                })?;
            for ty in arguments {
                sort(ty)?;
            }
            Ok(value(Kind::Container {
                name: sort(&schema)?,
                kind: kind.into(),
                arguments: arguments.to_vec(),
            }))
        }
        ("constructor", [constructor]) => {
            let Value::ConstructorFunction(name, _) = constructor else {
                return Err("constructor requires a checked enum constructor".into());
            };
            Ok(value(Kind::Constructor {
                name: name.rsplit("::").next().unwrap_or(name).into(),
            }))
        }
        ("forall", [body]) => {
            let Type::Function(parameters, _) = parameter("F")? else {
                return Err("forall requires a function".into());
            };
            for (_, ty) in &parameters {
                if expression_sort(ty).is_none() {
                    return Err("forall parameters must be supported Expr<T> values".into());
                }
            }
            let scope = evaluator.fresh_symbolic_scope();
            let symbolic = (0..parameters.len())
                .map(|index| value(Kind::Expr(Expression::Variable(scope, index))))
                .collect();
            let completed = evaluator
                .call(source, span, body.clone(), symbolic)
                .map_err(|error| error.to_string())?;
            let body = goal(record_field(&completed, "when")?)?;
            let head = goal(record_field(&completed, "then")?)?;
            if head
                .iter()
                .any(|fact| !matches!(fact, Fact::Atom(..) | Fact::Erase(..)))
            {
                return Err("a rule head must contain positive relation or table rows".into());
            }
            if body.iter().any(|fact| matches!(fact, Fact::Erase(..))) {
                return Err("erase is only valid in a rule head".into());
            }
            let body_variables = variables(&body);
            let head_variables = variables(&head);
            if body_variables
                .iter()
                .chain(&head_variables)
                .any(|(owner, _)| *owner != scope)
            {
                return Err("a quantified variable escaped its forall scope".into());
            }
            if !head_variables.is_subset(&body_variables) {
                return Err("a rule head contains an unbound variable".into());
            }
            Ok(value(Kind::Clause { body, head }))
        }
        ("saturate", [clauses]) => Ok(value(Kind::Stage(
            collection(clauses)?
                .iter()
                .map(|clause| {
                    let clause = native(clause)?;
                    if !matches!(clause.0, Kind::Clause { .. }) {
                        return Err("saturate requires completed clauses".into());
                    }
                    Ok(clause.clone())
                })
                .collect::<Result<_, String>>()?,
        ))),
        ("query_result", [relation]) => {
            Ok(value(Kind::Result(Box::new(native(relation)?.clone()))))
        }
        ("program", [description]) => {
            let tables = collection(record_field(description, "tables")?)?
                .iter()
                .map(|table| native(table).cloned())
                .collect::<Result<Vec<_>, _>>()?;
            let stages = collection(record_field(description, "stages")?)?
                .iter()
                .map(|stage| native(stage).cloned())
                .collect::<Result<Vec<_>, _>>()?;
            let Kind::Result(result) = &native(record_field(description, "result")?)?.0 else {
                return Err("program requires an explicit result contract".into());
            };
            let Kind::Relation {
                key: result_key,
                schema: result_schema,
            } = &result.0
            else {
                return Err("query result requires a relation".into());
            };
            if !tables.iter().any(|table| matches!(&table.0, Kind::Relation { key, schema } if key == result_key && schema == result_schema)) { return Err("query result relation is not declared by the program".into()); }
            for (field, marker) in [
                ("completion", "fixed_point"),
                ("publication", "analysis_only"),
            ] {
                if !matches!(&native(record_field(description,field)?)?.0, Kind::Marker(value) if value == marker)
                {
                    return Err(format!("unsupported program {field}"));
                }
            }
            validate_program(&tables, &stages)?;
            Ok(value(Kind::Program {
                tables,
                stages,
                result: result.clone(),
            }))
        }
        _ => Err(format!("unavailable trusted relational function `{name}`")),
    }
}
fn variables(facts: &[Fact]) -> BTreeSet<(u64, usize)> {
    fn collect(expression: &Expression, result: &mut BTreeSet<(u64, usize)>) {
        match expression {
            Expression::Variable(scope, index) => {
                result.insert((*scope, *index));
            }
            Expression::Call(_, arguments) => {
                for argument in arguments {
                    collect(argument, result);
                }
            }
            _ => {}
        }
    }
    let mut result = BTreeSet::new();
    for fact in facts {
        match fact {
            Fact::Atom(_, args, _) | Fact::Erase(_, args, _) | Fact::Predicate(_, args) => {
                for arg in args {
                    collect(arg, &mut result);
                }
            }
            Fact::Equal(left, right) => {
                collect(left, &mut result);
                collect(right, &mut result);
            }
        }
    }
    result
}
fn validate_program(tables: &[RelationalValue], stages: &[RelationalValue]) -> Result<(), String> {
    let mut schemas = BTreeMap::new();
    for table in tables {
        let (key, schema) = declaration(table)?;
        if let Some(previous) = schemas.insert(key, schema.clone())
            && previous != schema
        {
            return Err(format!("incompatible declarations for relation `{key}`"));
        }
    }
    for stage in stages {
        let Kind::Stage(clauses) = &stage.0 else {
            return Err("program stages must be scheduling descriptions".into());
        };
        for clause in clauses {
            let Kind::Clause { body, head, .. } = &clause.0 else {
                return Err("invalid completed clause".into());
            };
            for fact in body.iter().chain(head) {
                if let Fact::Atom(key, _, schema) | Fact::Erase(key, _, schema) = fact {
                    match schemas.get(key.as_str()) {
                        None => return Err(format!("undeclared relation `{key}`")),
                        Some(declared) if declared != schema => {
                            return Err(format!("incompatible declarations for relation `{key}`"));
                        }
                        Some(_) => {}
                    }
                }
            }
        }
    }
    Ok(())
}

/// Lower a checked relational program without evaluating an engine schedule.
pub fn emit(module: &CheckedModule, binding: &str) -> Result<String, Diagnostic> {
    let evaluated = module.evaluate(binding)?;
    let library = native(&evaluated).map_err(|message| error(binding, &(0..0), message))?;
    let Kind::Program {
        tables,
        stages,
        result,
    } = &library.0
    else {
        return Err(error(binding, &(0..0), "expected a relational program"));
    };
    if !matches!(result.0, Kind::Relation { .. }) {
        return Err(error(binding, &(0..0), "unsupported program result"));
    }
    let mut output = String::new();
    let mut emitted = BTreeSet::new();
    for table in tables {
        let (key, _) = declaration(table).map_err(|message| error(binding, &(0..0), message))?;
        if !emitted.insert(key) {
            continue;
        }
        emit_declaration(table, &mut output).map_err(|message| error(binding, &(0..0), message))?;
    }
    for (index, stage) in stages.iter().enumerate() {
        let name = format!("stage{index}");
        output.push_str(&format!("(ruleset {name})\n"));
        let Kind::Stage(clauses) = &stage.0 else {
            return Err(error(binding, &(0..0), "invalid stage"));
        };
        for clause in clauses {
            emit_clause(clause, &name, &mut output)
                .map_err(|message| error(binding, &(0..0), message))?;
        }
    }
    Ok(output)
}
fn sort(ty: &Type) -> Result<String, String> {
    match ty {
        Type::Integer | Type::Nat => Ok("i64".into()),
        Type::String => Ok("String".into()),
        Type::Bool => Ok("bool".into()),
        Type::Nominal(name, arguments) if arguments.is_empty() => {
            Ok(name.rsplit("::").next().unwrap_or(name).into())
        }
        _ => Err(format!("unsupported engine sort `{ty}`")),
    }
}
fn emit_declaration(value: &RelationalValue, output: &mut String) -> Result<(), String> {
    let types = |types: &[Type]| {
        types
            .iter()
            .map(sort)
            .collect::<Result<Vec<_>, _>>()
            .map(|types| types.join(" "))
    };
    match &value.0 {
        Kind::Relation { key, schema } => {
            output.push_str(&format!("(relation {key} ({}))\n", types(schema)?))
        }
        Kind::Table {
            key,
            keys,
            value,
            merge,
        } => output.push_str(&format!(
            "(function {key} ({}) {} {})\n",
            types(keys)?,
            sort(value)?,
            merge
                .as_ref()
                .map(|merge| format!(":merge {}", expression_text(merge)))
                .unwrap_or_else(|| ":no-merge".into())
        )),
        _ => return Err("expected a relation or table declaration".into()),
    }
    Ok(())
}
fn expression_text(expression: &Expression) -> String {
    match expression {
        Expression::Variable(_, index) => format!("v{index}"),
        Expression::MergeOld => "old".into(),
        Expression::MergeNew => "new".into(),
        Expression::Integer(value) => value.to_string(),
        Expression::String(value) => format!("{value:?}"),
        Expression::Bool(value) => value.to_string(),
        Expression::Call(name, args) => call_text(name, args),
    }
}
fn call_text(name: &str, args: &[Expression]) -> String {
    let args = args
        .iter()
        .map(expression_text)
        .collect::<Vec<_>>()
        .join(" ");
    if args.is_empty() {
        format!("({name})")
    } else {
        format!("({name} {args})")
    }
}
fn fact_text(fact: &Fact, head: bool) -> String {
    match fact {
        Fact::Erase(name, args, _) => format!("(delete {})", call_text(name, args)),
        Fact::Predicate(name, args) => call_text(name, args),
        Fact::Equal(left, right) => {
            format!("(= {} {})", expression_text(left), expression_text(right))
        }
        Fact::Atom(name, args, Schema::Relation(_)) => call_text(name, args),
        Fact::Atom(name, args, Schema::Table(..)) => {
            let key = &args[..args.len() - 1];
            let result = expression_text(&args[args.len() - 1]);
            let call = call_text(name, key);
            if head {
                format!("(set {call} {result})")
            } else {
                format!("(= {call} {result})")
            }
        }
    }
}
fn emit_clause(clause: &RelationalValue, ruleset: &str, output: &mut String) -> Result<(), String> {
    let Kind::Clause { body, head } = &clause.0 else {
        return Err("expected a completed quantified clause".into());
    };
    output.push_str(&format!(
        "(rule ({}) ({}) :ruleset {ruleset})\n",
        body.iter()
            .map(|fact| fact_text(fact, false))
            .collect::<Vec<_>>()
            .join(" "),
        head.iter()
            .map(|fact| fact_text(fact, true))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    Ok(())
}

/// Ordered build metadata selecting checked declarations and completed clauses.
/// This metadata preserves legacy engine names and grouping outside CDSL programs.
#[derive(Clone, Copy, Debug)]
pub enum SchemaEntry<'a> {
    /// A named engine ruleset.
    Ruleset(&'a str),
    /// An engine ruleset combining existing rulesets.
    CombinedRuleset(&'a str, &'a [&'a str]),
    /// An authoritative checked inductive group.
    Datatypes(&'a [&'a str]),
    /// A binding evaluating to a relation or table declaration.
    Declaration(&'a str),
    /// A binding evaluating to a typed container sort declaration.
    Container(&'a str),
    /// A completed clause binding and its engine ruleset.
    Rule(&'a str, &'a str),
    /// A list of completed clauses emitted in order into one engine ruleset.
    Rules(&'a str, &'a str),
}

/// Lower selected checked schema exports to the existing engine input format.
pub fn emit_schema(
    module: &CheckedModule,
    entries: &[SchemaEntry<'_>],
) -> Result<String, Diagnostic> {
    emit_schema_extension(module, &[], entries)
}

/// Lower checked exports against declarations already installed by an imported schema.
pub fn emit_schema_extension(
    module: &CheckedModule,
    imports: &[SchemaEntry<'_>],
    entries: &[SchemaEntry<'_>],
) -> Result<String, Diagnostic> {
    let mut exports = BTreeMap::new();
    for entry in imports.iter().chain(entries) {
        if let SchemaEntry::Declaration(binding)
        | SchemaEntry::Container(binding)
        | SchemaEntry::Rule(binding, _)
        | SchemaEntry::Rules(binding, _) = entry
            && let std::collections::btree_map::Entry::Vacant(entry) = exports.entry(*binding)
        {
            entry.insert(module.evaluate(binding)?);
        }
    }
    let mut declarations = Vec::new();
    let mut clauses = Vec::new();
    for entry in imports.iter().chain(entries) {
        match entry {
            SchemaEntry::Declaration(binding) => declarations.push(
                native(&exports[binding])
                    .map_err(|message| error(binding, &(0..0), message))?
                    .clone(),
            ),
            SchemaEntry::Rule(binding, _) => clauses.push(
                native(&exports[binding])
                    .map_err(|message| error(binding, &(0..0), message))?
                    .clone(),
            ),
            SchemaEntry::Rules(binding, _) => {
                for clause in collection(&exports[binding])
                    .map_err(|message| error(binding, &(0..0), message))?
                {
                    clauses.push(
                        native(clause)
                            .map_err(|message| error(binding, &(0..0), message))?
                            .clone(),
                    );
                }
            }
            _ => {}
        }
    }
    validate_program(&declarations, &[RelationalValue(Kind::Stage(clauses))])
        .map_err(|message| error("relational schema", &(0..0), message))?;
    let mut output = String::new();
    let mut emitted = BTreeSet::new();
    for entry in entries {
        let result = match entry {
            SchemaEntry::Ruleset(name) => {
                output.push_str(&format!("(ruleset {name})\n"));
                Ok(())
            }
            SchemaEntry::CombinedRuleset(name, members) => {
                output.push_str(&format!(
                    "(unstable-combined-ruleset {name} {})\n",
                    members.join(" ")
                ));
                Ok(())
            }
            SchemaEntry::Datatypes(names) => emit_datatypes(module, names, &mut output),
            SchemaEntry::Declaration(binding) => native(&exports[binding]).and_then(|value| {
                let (key, _) = declaration(value)?;
                if emitted.insert(key) {
                    emit_declaration(value, &mut output)
                } else {
                    Ok(())
                }
            }),
            SchemaEntry::Rule(binding, ruleset) => {
                native(&exports[binding]).and_then(|value| emit_clause(value, ruleset, &mut output))
            }
            SchemaEntry::Rules(binding, ruleset) => {
                collection(&exports[binding]).and_then(|clauses| {
                    for clause in clauses {
                        emit_clause(native(clause)?, ruleset, &mut output)?;
                    }
                    Ok(())
                })
            }
            SchemaEntry::Container(binding) => native(&exports[binding]).and_then(|value| {
                let Kind::Container {
                    name,
                    kind,
                    arguments,
                } = &value.0
                else {
                    return Err("expected a typed container declaration".into());
                };
                if emitted.insert(name) {
                    let arguments = arguments.iter().map(sort).collect::<Result<Vec<_>, _>>()?;
                    output.push_str(&format!("(sort {name} ({kind} {}))\n", arguments.join(" ")));
                }
                Ok(())
            }),
        };
        result.map_err(|message| error("relational schema", &(0..0), message))?;
    }
    Ok(output)
}
fn emit_datatypes(
    module: &CheckedModule,
    names: &[&str],
    output: &mut String,
) -> Result<(), String> {
    if names.is_empty() {
        return Err("a datatype group must contain a checked enum".into());
    }
    let mut declarations = Vec::new();
    for name in names {
        let constructors = module
            .inductive_constructors(name)
            .ok_or_else(|| format!("unknown checked enum `{name}`"))?;
        let mut declaration = name.rsplit("::").next().unwrap_or(name).to_string();
        for (constructor, fields) in constructors {
            let fields = fields.iter().map(sort).collect::<Result<Vec<_>, _>>()?;
            declaration.push_str(&format!(
                " ({}{})",
                constructor.rsplit("::").next().unwrap_or(constructor),
                if fields.is_empty() {
                    String::new()
                } else {
                    format!(" {}", fields.join(" "))
                }
            ));
        }
        declarations.push(declaration);
    }
    if declarations.len() == 1 {
        output.push_str(&format!("(datatype {})\n", declarations[0]));
    } else {
        output.push_str(&format!(
            "(datatype* {})\n",
            declarations
                .iter()
                .map(|declaration| format!("({declaration})"))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    Ok(())
}
