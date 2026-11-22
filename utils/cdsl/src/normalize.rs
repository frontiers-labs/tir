//! Capture-avoiding beta reduction for compile-time expressions.
use crate::{Error, ast::*};
use std::collections::{BTreeMap, BTreeSet};

type Values = BTreeMap<Path, Expr>;

pub fn substitute(expr: &Expr, values: &Values) -> Result<Expr, Error> {
    let mut next = 0;
    for expression in std::iter::once(expr).chain(values.values()) {
        crate::totality::visit(expression, &mut |e| {
            if let Expression::Path(path) = &e.value {
                for name in path {
                    if let Some(id) = name
                        .strip_prefix("\0cdsl")
                        .and_then(|n| n.parse::<usize>().ok())
                    {
                        next = next.max(id.saturating_add(1));
                    }
                }
            }
            Ok(())
        })?;
    }
    crate::totality::visit(expr, &mut |e| {
        if let Expression::Record(fields) = &e.value {
            let mut prior = std::collections::BTreeSet::new();
            for (name, field) in fields {
                crate::totality::visit(field, &mut |part| {
                    if let Expression::Path(path) = &part.value
                        && !prior.contains(path)
                        && let Some(replacement) = values.get(path)
                    {
                        crate::totality::visit(replacement, &mut |value| {
                            if matches!(&value.value,Expression::Path(p) if prior.contains(p)) {
                                return Err(Error("dependent record substitution would capture a field; rename the field or supply equality evidence".into()));
                            }
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
                prior.insert(vec![name.clone()]);
            }
        }
        Ok(())
    })?;
    Ok(replace(expr, values, &mut next))
}
fn replacement(path: &Path, range: &std::ops::Range<usize>) -> Expr {
    Node {
        value: Expression::Path(path.clone()),
        range: range.clone(),
    }
}
fn bind(binder: &Binder, values: &mut Values, next: &mut usize) -> Binder {
    let ty = replace(&binder.ty, values, next);
    let path = vec![format!("\0cdsl{}", *next)];
    *next += 1;
    values.insert(
        vec![binder.name.clone()],
        replacement(&path, &binder.ty.range),
    );
    Binder {
        name: path[0].clone(),
        ty,
    }
}
fn binding(binding: &Binding, values: &mut Values, next: &mut usize) -> Binding {
    let mut nested = values.clone();
    let generics = binding
        .generics
        .iter()
        .map(|b| bind(b, &mut nested, next))
        .collect();
    let ty = binding.ty.as_ref().map(|t| replace(t, &nested, next));
    let value = replace(&binding.value, &nested, next);
    let path = vec![format!("\0cdsl{}", *next)];
    *next += 1;
    values.insert(
        binding.path.clone(),
        replacement(&path, &binding.value.range),
    );
    Binding {
        path,
        generics,
        ty,
        value,
    }
}
fn block(block: &Block, values: &Values, next: &mut usize) -> Block {
    let mut values = values.clone();
    let bindings = block
        .bindings
        .iter()
        .map(|b| binding(b, &mut values, next))
        .collect();
    Block {
        bindings,
        value: Box::new(replace(&block.value, &values, next)),
    }
}
fn pattern(p: &Pattern, values: &mut Values, next: &mut usize) -> Pattern {
    let value = match &p.value {
        PatternKind::Path(path) if path.len() == 1 => {
            let new = vec![format!("\0cdsl{}", *next)];
            *next += 1;
            values.insert(path.clone(), replacement(&new, &p.range));
            PatternKind::Path(new)
        }
        PatternKind::Constructor { path, arguments } => PatternKind::Constructor {
            path: path.clone(),
            arguments: arguments.iter().map(|p| pattern(p, values, next)).collect(),
        },
        PatternKind::Tuple(ps) => {
            PatternKind::Tuple(ps.iter().map(|p| pattern(p, values, next)).collect())
        }
        value => value.clone(),
    };
    Node {
        value,
        range: p.range.clone(),
    }
}
fn replace(e: &Expr, values: &Values, next: &mut usize) -> Expr {
    let value = match &e.value {
        Expression::Path(path) => return values.get(path).cloned().unwrap_or_else(|| e.clone()),
        Expression::Literal(_) | Expression::Unit => e.value.clone(),
        Expression::Tuple(es) => {
            Expression::Tuple(es.iter().map(|e| replace(e, values, next)).collect())
        }
        Expression::Array(es) => {
            Expression::Array(es.iter().map(|e| replace(e, values, next)).collect())
        }
        Expression::Record(fields) => {
            let mut local = values.clone();
            let mut output = Vec::new();
            for (name, e) in fields {
                output.push((name.clone(), replace(e, &local, next)));
                local.remove(&vec![name.clone()]);
            }
            Expression::Record(output)
        }
        Expression::Rule { binders, clause } => {
            let mut local = values.clone();
            let binders = binders
                .iter()
                .map(|binder| {
                    let ty = binder.ty.as_ref().map(|ty| replace(ty, &local, next));
                    let path = vec![format!("\0cdsl{}", *next)];
                    *next += 1;
                    local.insert(vec![binder.name.clone()], replacement(&path, &clause.range));
                    RuleBinder {
                        name: path[0].clone(),
                        ty,
                    }
                })
                .collect();
            Expression::Rule {
                binders,
                clause: Box::new(replace(clause, &local, next)),
            }
        }
        Expression::LocalTable(ty) => Expression::LocalTable(Box::new(replace(ty, values, next))),
        Expression::Run(schedule) => Expression::Run(Box::new(replace(schedule, values, next))),
        Expression::Block(b) => Expression::Block(block(b, values, next)),
        Expression::Function(f) => {
            let mut local = values.clone();
            let generics = f
                .generics
                .iter()
                .map(|b| bind(b, &mut local, next))
                .collect();
            let parameters = f
                .parameters
                .iter()
                .map(|b| bind(b, &mut local, next))
                .collect();
            Expression::Function(Box::new(Function {
                memo: f.memo,
                generics,
                parameters,
                result: f.result.as_ref().map(|e| replace(e, &local, next)),
                decreases: f.decreases.as_ref().map(|e| replace(e, &local, next)),
                body: block(&f.body, &local, next),
            }))
        }
        Expression::If {
            condition,
            then,
            otherwise,
        } => Expression::If {
            condition: Box::new(replace(condition, values, next)),
            then: block(then, values, next),
            otherwise: Box::new(replace(otherwise, values, next)),
        },
        Expression::Match { value, arms } => Expression::Match {
            value: Box::new(replace(value, values, next)),
            arms: arms
                .iter()
                .map(|(p, e)| {
                    let mut local = values.clone();
                    let p = pattern(p, &mut local, next);
                    (p, replace(e, &local, next))
                })
                .collect(),
        },
        Expression::Unary { op, value } => Expression::Unary {
            op: *op,
            value: Box::new(replace(value, values, next)),
        },
        Expression::Binary { op, left, right } => Expression::Binary {
            op: *op,
            left: Box::new(replace(left, values, next)),
            right: Box::new(replace(right, values, next)),
        },
        Expression::Generic { value, arguments } => Expression::Generic {
            value: Box::new(replace(value, values, next)),
            arguments: arguments.iter().map(|e| replace(e, values, next)).collect(),
        },
        Expression::Call { value, arguments } => Expression::Call {
            value: Box::new(replace(value, values, next)),
            arguments: arguments.iter().map(|e| replace(e, values, next)).collect(),
        },
        Expression::Member { value, name } => Expression::Member {
            value: Box::new(replace(value, values, next)),
            name: name.clone(),
        },
        Expression::Op(fields) => Expression::Op(
            fields
                .iter()
                .map(|field| Node {
                    value: match &field.value {
                        OpField::Field { name, value } => OpField::Field {
                            name: name.clone(),
                            value: replace(value, values, next),
                        },
                        OpField::Interfaces { receiver, entries } => {
                            let mut scope = values.clone();
                            let renamed = vec![format!("\0cdsl{}", *next)];
                            *next += 1;
                            scope.insert(
                                vec![receiver.clone()],
                                replacement(&renamed, &field.range),
                            );
                            OpField::Interfaces {
                                receiver: renamed[0].clone(),
                                entries: entries
                                    .iter()
                                    .map(|(name, path, methods)| {
                                        (name.clone(), path.clone(), replace(methods, &scope, next))
                                    })
                                    .collect(),
                            }
                        }
                        OpField::Signature {
                            parameters,
                            result,
                            results,
                        } => OpField::Signature {
                            parameters: parameters
                                .iter()
                                .map(|parameter| Binder {
                                    name: parameter.name.clone(),
                                    ty: replace(&parameter.ty, values, next),
                                })
                                .collect(),
                            result: replace(result, values, next),
                            results: results.clone(),
                        },
                    },
                    range: field.range.clone(),
                })
                .collect(),
        ),
        Expression::Ir { bindings, value } => {
            let body = block(
                &Block {
                    bindings: bindings.clone(),
                    value: value.clone(),
                },
                values,
                next,
            );
            Expression::Ir {
                bindings: body.bindings,
                value: body.value,
            }
        }
        Expression::RecordCall {
            value,
            fields,
            arguments,
        } => Expression::RecordCall {
            value: Box::new(replace(value, values, next)),
            fields: fields
                .iter()
                .map(|(name, value)| (name.clone(), replace(value, values, next)))
                .collect(),
            arguments: arguments
                .iter()
                .map(|argument| replace(argument, values, next))
                .collect(),
        },
        Expression::Limit(fields) => Expression::Limit(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), replace(value, values, next)))
                .collect(),
        ),
    };
    Node {
        value,
        range: e.range.clone(),
    }
}

pub fn head(
    expr: &Expr,
    definitions: &BTreeMap<Path, &Binding>,
    constructors: &BTreeSet<Path>,
) -> Result<Expr, Error> {
    match &expr.value {
        Expression::Op(fields) => {
            if fields.iter().any(|field| {
                matches!(&field.value,
                OpField::Interfaces { receiver, .. } if receiver == "self")
            }) {
                return head(
                    &substitute(expr, &Values::new())?,
                    definitions,
                    constructors,
                );
            }
            crate::infer::sugar::operation_record(fields, expr.range.clone())
        }
        Expression::Ir { bindings, value } => head(
            &Node {
                value: Expression::Block(Block {
                    bindings: bindings.clone(),
                    value: value.clone(),
                }),
                range: expr.range.clone(),
            },
            definitions,
            constructors,
        ),
        Expression::Limit(fields) => Ok(Node {
            value: Expression::Record(fields.clone()),
            range: expr.range.clone(),
        }),
        Expression::RecordCall {
            value,
            fields,
            arguments,
        } => {
            let mut supplied = vec![Node {
                value: Expression::Record(fields.clone()),
                range: expr.range.clone(),
            }];
            supplied.extend_from_slice(arguments);
            head(
                &Node {
                    value: Expression::Call {
                        value: value.clone(),
                        arguments: supplied,
                    },
                    range: expr.range.clone(),
                },
                definitions,
                constructors,
            )
        }
        Expression::Path(path) => {
            if let Some(binding) = definitions.get(path)
                && binding.generics.is_empty()
            {
                return head(&binding.value, definitions, constructors);
            }
            Ok(expr.clone())
        }
        Expression::Block(block) => {
            let mut values = Values::new();
            for binding in &block.bindings {
                if !binding.generics.is_empty() {
                    return Err(Error(
                        "type-level local generics need explicit specialization".into(),
                    ));
                }
                values.insert(
                    binding.path.clone(),
                    head(
                        &substitute(&binding.value, &values)?,
                        definitions,
                        constructors,
                    )?,
                );
            }
            head(
                &substitute(&block.value, &values)?,
                definitions,
                constructors,
            )
        }
        Expression::Generic { value, arguments } => {
            if let Expression::Path(path) = &value.value
                && let Some(binding) = definitions.get(path)
            {
                let (parameters, body) = if !binding.generics.is_empty() {
                    (&binding.generics, binding.value.clone())
                } else if let Expression::Function(function) = &binding.value.value {
                    let mut function = function.clone();
                    let parameters = function.generics.clone();
                    function.generics.clear();
                    if parameters.len() != arguments.len() {
                        return Err(Error("generic normalization arity mismatch".into()));
                    }
                    let mut values = parameters
                        .iter()
                        .zip(arguments)
                        .map(|(b, e)| (vec![b.name.clone()], e.clone()))
                        .collect::<Values>();
                    values.insert(path.clone(), expr.clone());
                    return head(
                        &substitute(
                            &Node {
                                value: Expression::Function(function),
                                range: expr.range.clone(),
                            },
                            &values,
                        )?,
                        definitions,
                        constructors,
                    );
                } else {
                    return Ok(expr.clone());
                };
                if parameters.len() != arguments.len() {
                    return Err(Error("generic normalization arity mismatch".into()));
                }
                let mut values = parameters
                    .iter()
                    .zip(arguments)
                    .map(|(b, e)| (vec![b.name.clone()], e.clone()))
                    .collect::<Values>();
                values.insert(path.clone(), expr.clone());
                return head(&substitute(&body, &values)?, definitions, constructors);
            }
            let value = head(value, definitions, constructors)?;
            let Expression::Function(mut function) = value.value else {
                return Ok(expr.clone());
            };
            if function.generics.len() != arguments.len() {
                return Err(Error("generic normalization arity mismatch".into()));
            }
            let values = function
                .generics
                .iter()
                .zip(arguments)
                .map(|(b, e)| (vec![b.name.clone()], e.clone()))
                .collect();
            function.generics.clear();
            head(
                &substitute(
                    &Node {
                        value: Expression::Function(function),
                        range: expr.range.clone(),
                    },
                    &values,
                )?,
                definitions,
                constructors,
            )
        }
        Expression::Call { value, arguments } => {
            if let Expression::Path(path) = &value.value
                && matches!(path.join("::").as_str(), "lt" | "gt")
            {
                let [a, b] = arguments.as_slice() else {
                    return Err(Error("comparison needs two arguments".into()));
                };
                if let (Some(a), Some(b)) = (
                    natural_expression(a, definitions, constructors)?,
                    natural_expression(b, definitions, constructors)?,
                ) {
                    return Ok(Node {
                        value: Expression::Literal(Literal::Bool(if path[0] == "lt" {
                            a < b
                        } else {
                            a > b
                        })),
                        range: expr.range.clone(),
                    });
                }
                return Ok(expr.clone());
            }
            let value = head(value, definitions, constructors)?;
            let Expression::Function(function) = &value.value else {
                return Ok(Node {
                    value: Expression::Call {
                        value: Box::new(value),
                        arguments: arguments
                            .iter()
                            .map(|e| head(e, definitions, constructors))
                            .collect::<Result<_, _>>()?,
                    },
                    range: expr.range.clone(),
                });
            };
            if function.parameters.len() != arguments.len() || !function.generics.is_empty() {
                return Err(Error("type computation application arity mismatch".into()));
            }
            let values = function
                .parameters
                .iter()
                .zip(arguments)
                .map(|(b, e)| (vec![b.name.clone()], e.clone()))
                .collect();
            let body = Node {
                value: Expression::Block(function.body.clone()),
                range: expr.range.clone(),
            };
            head(&substitute(&body, &values)?, definitions, constructors)
        }
        Expression::Match { value, arms } => {
            let value = head(value, definitions, constructors)?;
            for (pattern, body) in arms {
                let mut values = Values::new();
                match matches_pattern(pattern, &value, &mut values, definitions, constructors)? {
                    Some(true) => {
                        return head(&substitute(body, &values)?, definitions, constructors);
                    }
                    Some(false) => {}
                    None => {
                        return Err(Error(
                            "type-level match is neutral; supply explicit equality evidence".into(),
                        ));
                    }
                }
            }
            Err(Error(
                "no constructor case applies during normalization".into(),
            ))
        }
        Expression::Member { value, name } => {
            let value = head(value, definitions, constructors)?;
            if let Expression::Record(fields) = &value.value
                && let Some((_, field)) = fields.iter().find(|(field, _)| field == name)
            {
                return head(field, definitions, constructors);
            }
            Ok(Node {
                value: Expression::Member {
                    value: Box::new(value),
                    name: name.clone(),
                },
                range: expr.range.clone(),
            })
        }
        Expression::Binary { op, left, right } => {
            let left = head(left, definitions, constructors)?;
            let right = head(right, definitions, constructors)?;
            let value = match (op, &left.value, &right.value) {
                (BinaryOp::Equal, _, _)
                    if format!("{:?}", left.value) == format!("{:?}", right.value) =>
                {
                    Some(Literal::Bool(true))
                }
                (BinaryOp::NotEqual, _, _)
                    if format!("{:?}", left.value) == format!("{:?}", right.value) =>
                {
                    Some(Literal::Bool(false))
                }
                (
                    BinaryOp::And,
                    Expression::Literal(Literal::Bool(a)),
                    Expression::Literal(Literal::Bool(b)),
                ) => Some(Literal::Bool(*a && *b)),
                (
                    BinaryOp::Or,
                    Expression::Literal(Literal::Bool(a)),
                    Expression::Literal(Literal::Bool(b)),
                ) => Some(Literal::Bool(*a || *b)),
                (
                    BinaryOp::Equal | BinaryOp::NotEqual,
                    Expression::Literal(Literal::Natural(a)),
                    Expression::Literal(Literal::Natural(b)),
                ) => {
                    let same = a.replace('_', "").trim_start_matches('0')
                        == b.replace('_', "").trim_start_matches('0');
                    Some(Literal::Bool(if matches!(op, BinaryOp::Equal) {
                        same
                    } else {
                        !same
                    }))
                }
                (
                    BinaryOp::Equal | BinaryOp::NotEqual,
                    Expression::Literal(Literal::Bool(a)),
                    Expression::Literal(Literal::Bool(b)),
                ) => Some(Literal::Bool(if matches!(op, BinaryOp::Equal) {
                    a == b
                } else {
                    a != b
                })),
                (
                    BinaryOp::Equal | BinaryOp::NotEqual,
                    Expression::Literal(Literal::String(a)),
                    Expression::Literal(Literal::String(b)),
                ) => Some(Literal::Bool(if matches!(op, BinaryOp::Equal) {
                    a == b
                } else {
                    a != b
                })),
                // Arithmetic needs its checked numeric domain; beta reduction cannot choose Nat or Int semantics.
                _ => None,
            };
            Ok(Node {
                value: if let Some(value) = value {
                    Expression::Literal(value)
                } else {
                    Expression::Binary {
                        op: *op,
                        left: Box::new(left),
                        right: Box::new(right),
                    }
                },
                range: expr.range.clone(),
            })
        }
        Expression::Unary {
            op: UnaryOp::Not,
            value,
        } => {
            let value = head(value, definitions, constructors)?;
            Ok(Node {
                value: match value.value {
                    Expression::Literal(Literal::Bool(b)) => Expression::Literal(Literal::Bool(!b)),
                    _ => Expression::Unary {
                        op: UnaryOp::Not,
                        value: Box::new(value),
                    },
                },
                range: expr.range.clone(),
            })
        }
        Expression::If {
            condition,
            then,
            otherwise,
        } => {
            let condition = head(condition, definitions, constructors)?;
            match condition.value {
                Expression::Literal(Literal::Bool(true)) => head(
                    &Node {
                        value: Expression::Block(then.clone()),
                        range: expr.range.clone(),
                    },
                    definitions,
                    constructors,
                ),
                Expression::Literal(Literal::Bool(false)) => {
                    head(otherwise, definitions, constructors)
                }
                _ => Ok(Node {
                    value: Expression::If {
                        condition: Box::new(condition),
                        then: then.clone(),
                        otherwise: otherwise.clone(),
                    },
                    range: expr.range.clone(),
                }),
            }
        }
        _ => Ok(expr.clone()),
    }
}

fn constructor_value<'a>(
    expr: &'a Expr,
    constructors: &BTreeSet<Path>,
) -> Option<(&'a Path, &'a [Expr])> {
    let (value, arguments) = match &expr.value {
        Expression::Call { value, arguments } => (&value.value, arguments.as_slice()),
        value => (value, &[][..]),
    };
    let value = if let Expression::Generic { value, .. } = value {
        &value.value
    } else {
        value
    };
    if let Expression::Path(path) = value
        && constructors.contains(path)
    {
        Some((path, arguments))
    } else {
        None
    }
}
fn matches_pattern(
    pattern: &Pattern,
    value: &Expr,
    values: &mut Values,
    definitions: &BTreeMap<Path, &Binding>,
    constructors: &BTreeSet<Path>,
) -> Result<Option<bool>, Error> {
    let value = head(value, definitions, constructors)?;
    let result = match (&pattern.value, &value.value) {
        (PatternKind::Wildcard, _) => Some(true),
        (PatternKind::Path(path), _) if path.len() == 1 => {
            values.insert(path.clone(), value.clone());
            Some(true)
        }
        (PatternKind::Path(path), _) => constructor_value(&value, constructors)
            .map(|(actual, args)| actual == path && args.is_empty()),
        (PatternKind::Unit, Expression::Unit) => Some(true),
        (PatternKind::Literal(Literal::Bool(a)), Expression::Literal(Literal::Bool(b))) => {
            Some(a == b)
        }
        (PatternKind::Literal(Literal::String(a)), Expression::Literal(Literal::String(b))) => {
            Some(a == b)
        }
        (PatternKind::Literal(Literal::Natural(a)), Expression::Literal(Literal::Natural(b))) => {
            Some(
                a.replace('_', "").trim_start_matches('0')
                    == b.replace('_', "").trim_start_matches('0'),
            )
        }
        (PatternKind::Literal(Literal::Natural(pattern)), _) => {
            natural_expression(&value, definitions, constructors)?
                .map(|n| pattern.replace('_', "").trim_start_matches('0') == n.to_string())
        }
        (PatternKind::Tuple(patterns), Expression::Tuple(items)) => {
            return pattern_fields(patterns, items, values, definitions, constructors);
        }
        (PatternKind::Constructor { path, arguments }, _) => {
            if let Some((actual, items)) = constructor_value(&value, constructors) {
                if actual != path {
                    Some(false)
                } else {
                    return pattern_fields(arguments, items, values, definitions, constructors);
                }
            } else {
                None
            }
        }
        _ => None,
    };
    Ok(result)
}
fn pattern_fields(
    patterns: &[Pattern],
    items: &[Expr],
    values: &mut Values,
    definitions: &BTreeMap<Path, &Binding>,
    constructors: &BTreeSet<Path>,
) -> Result<Option<bool>, Error> {
    if patterns.len() != items.len() {
        return Ok(Some(false));
    }
    let mut neutral = false;
    for (pattern, item) in patterns.iter().zip(items) {
        match matches_pattern(pattern, item, values, definitions, constructors)? {
            Some(true) => {}
            Some(false) => return Ok(Some(false)),
            None => neutral = true,
        }
    }
    Ok(if neutral { None } else { Some(true) })
}

// Only call this after checking a Nat domain, such as a natural pattern or lt/gt argument.
fn natural_expression(
    expr: &Expr,
    definitions: &BTreeMap<Path, &Binding>,
    constructors: &BTreeSet<Path>,
) -> Result<Option<u128>, Error> {
    let value = head(expr, definitions, constructors)?;
    match &value.value {
        Expression::Literal(Literal::Natural(n)) => {
            Ok(Some(n.replace('_', "").parse().map_err(|_| {
                Error("natural normalization exceeds u128".into())
            })?))
        }
        Expression::Binary { op, left, right }
            if matches!(
                op,
                BinaryOp::Add
                    | BinaryOp::Subtract
                    | BinaryOp::Multiply
                    | BinaryOp::Divide
                    | BinaryOp::Remainder
            ) =>
        {
            let (Some(a), Some(b)) = (
                natural_expression(left, definitions, constructors)?,
                natural_expression(right, definitions, constructors)?,
            ) else {
                return Ok(None);
            };
            let result = match op {
                BinaryOp::Add => a.checked_add(b),
                BinaryOp::Subtract => Some(a.saturating_sub(b)),
                BinaryOp::Multiply => a.checked_mul(b),
                BinaryOp::Divide => Some(a.checked_div(b).unwrap_or(0)),
                BinaryOp::Remainder => Some(a.checked_rem(b).unwrap_or(a)),
                _ => None,
            };
            Ok(Some(result.ok_or_else(|| {
                Error("natural normalization exceeds u128".into())
            })?))
        }
        _ => Ok(None),
    }
}
