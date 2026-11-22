use super::*;
use tir_egglog::prelude::exprs::{call, string, var};

pub(super) fn carries_clause(expression: &Expr) -> bool {
    let mut found = false;
    let _ = crate::totality::visit(expression, &mut |expr| {
        found |= matches!(expr.value, Expression::Rule { .. });
        Ok(())
    });
    found
}

pub(super) fn collection_arguments(value: &Expr) -> Result<(&Expr, &Expr), Error> {
    if let Expression::Call { value, arguments } = &value.value {
        let callee = match &value.value {
            Expression::Generic { value, .. } => value,
            _ => value,
        };
        if path(callee).is_ok_and(|path| path == &["std", "rewrite", "collect"])
            && let [rules, policy] = arguments.as_slice()
        {
            return Ok((rules, policy));
        }
    }
    Err(Error(
        "a rule collection requires a tail call to std::rewrite::collect".into(),
    ))
}

pub(super) struct Policy {
    live: String,
    pinned_operation: String,
    pinned_value: String,
    replace: String,
    erase: String,
    definition: String,
    constant: String,
    attributes: String,
    attribute_integer: String,
    attributes_kind: TableKind,
}

impl Policy {
    pub(super) fn resolve(
        schema: &Schema,
        value: &Expr,
        values: &BTreeMap<Path, &Binding>,
        constructors: &BTreeSet<Path>,
    ) -> Result<Self, Error> {
        let Expression::Record(fields) = &value.value else {
            return Err(Error("rewrite policy requires a closed record".into()));
        };
        let named = |field: &str| {
            let expression = fields
                .iter()
                .find_map(|(name, expr)| (name == field).then_some(expr))
                .ok_or_else(|| Error(format!("rewrite policy is missing {field}")))?;
            let resolved = crate::normalize::head(expression, values, constructors)?;
            path(&resolved)
                .map(|path| schema.name(path))
                .map_err(|_| Error(format!("rewrite policy {field} requires a named callable")))
        };
        let engine = schema.database().schema();
        let table_kind = |name: &str| engine.table_named(name).map(|id| engine.table(id).kind());
        let relation = |field: &str| {
            let name = named(field)?;
            if table_kind(&name) == Some(TableKind::Relation) {
                Ok(name)
            } else {
                Err(Error(format!("rewrite policy {field} requires a relation")))
            }
        };
        let function = |field: &str| {
            let name = named(field)?;
            if table_kind(&name) == Some(TableKind::Constructor)
                || engine.primitives(&name).next().is_some()
            {
                Ok(name)
            } else {
                Err(Error(format!(
                    "rewrite policy {field} requires an executable function"
                )))
            }
        };
        let attributes = named("attributes")?;
        let attributes_kind = table_kind(&attributes)
            .filter(|kind| matches!(kind, TableKind::Relation | TableKind::Function))
            .ok_or_else(|| {
                Error("rewrite policy attributes requires a relation or table".into())
            })?;
        Ok(Self {
            live: relation("live")?,
            pinned_operation: relation("pinned_operation")?,
            pinned_value: relation("pinned_value")?,
            replace: relation("replace")?,
            erase: relation("erase")?,
            definition: relation("definition")?,
            constant: function("constant")?,
            attributes,
            attribute_integer: function("attribute_integer")?,
            attributes_kind,
        })
    }
}

pub(super) fn visit_rules(
    value: &Expr,
    owner: &Path,
    definitions: &BTreeMap<Path, Expr>,
    constructors: &BTreeSet<Path>,
    visit: &mut impl FnMut(&Path, &[RuleBinder], &Expr) -> Result<(), Error>,
) -> Result<(), Error> {
    let constructor = |expr: &Expr| {
        let expr = match &expr.value {
            Expression::Generic { value, .. } => value,
            _ => expr,
        };
        path(expr).is_ok_and(|path| constructors.contains(path))
    };
    match &value.value {
        Expression::Rule { binders, clause } => visit(owner, binders, clause),
        Expression::Path(path) if definitions.contains_key(path) => {
            visit_rules(&definitions[path], path, definitions, constructors, visit)
        }
        Expression::Call { value, arguments } if constructor(value) => {
            for argument in arguments {
                visit_rules(argument, owner, definitions, constructors, visit)?;
            }
            Ok(())
        }
        Expression::Tuple(items) | Expression::Array(items) => {
            for item in items {
                visit_rules(item, owner, definitions, constructors, visit)?;
            }
            Ok(())
        }
        Expression::Record(fields) => {
            for (_, value) in fields {
                visit_rules(value, owner, definitions, constructors, visit)?;
            }
            Ok(())
        }
        Expression::Path(_) | Expression::Generic { .. } if constructor(value) => Ok(()),
        Expression::Unit | Expression::Literal(_) => Ok(()),
        _ => Err(Error(
            "a rule collection requires a closed value of checked rules".into(),
        )),
    }
}

pub(super) fn native_operation<'a>(
    lowering: &RuleLowering<'a>,
    expression: &'a Expr,
) -> Option<(&'a OperationImport, &'a [Expr])> {
    let Expression::Call { value, arguments } = &expression.value else {
        return None;
    };
    let operation = lowering.operations.get(path(value).ok()?)?;
    Some((operation, arguments))
}

fn temporary(label: String, expr: &Expr) -> Expr {
    Expr {
        value: Expression::Path(vec![label]),
        range: expr.range.clone(),
    }
}

#[derive(Default)]
struct PatternFacts {
    facts: Vec<ast::Fact>,
    proposals: Vec<(std::ops::Range<usize>, ast::Fact)>,
    bound: BTreeSet<Path>,
}

impl PatternFacts {
    fn alternatives(self) -> Vec<ast::Facts> {
        let mut alternatives = vec![self.facts];
        for (range, proposal) in self.proposals.into_iter().rev() {
            let substituted = alternatives
                .iter()
                .map(|facts| {
                    let mut facts = facts.clone();
                    facts.splice(range.clone(), [proposal.clone()]);
                    facts
                })
                .collect::<Vec<_>>();
            alternatives.extend(substituted);
        }
        alternatives.into_iter().map(ast::Facts).collect()
    }
}

fn match_tree(
    lowering: &RuleLowering<'_>,
    policy: &Policy,
    expression: &Expr,
    result: &Expr,
    pattern: &mut PatternFacts,
    nested: bool,
) -> Result<ast::Expr, Error> {
    let (operation, arguments) = native_operation(lowering, expression)
        .ok_or_else(|| Error("a rewrite pattern requires a declared operation".into()))?;
    let checked = lowering
        .operation_matches
        .and_then(|matches| matches.get(&expression.range.start))
        .ok_or_else(|| Error("operation rewrite is missing checked ports".into()))?;
    let label = format!("@rewrite:{}", expression.range.start);
    let node = temporary(format!("{label}:node"), expression);
    let mut operands = Vec::new();
    if checked.literal_attribute.is_none() {
        for (index, argument) in arguments.iter().enumerate() {
            if let Expression::Path(path) = &argument.value {
                pattern.bound.insert(path.clone());
                operands.push(argument.clone());
            } else {
                let value = temporary(format!("{label}:input:{index}"), argument);
                match_tree(lowering, policy, argument, &value, pattern, true)?;
                operands.push(value);
            }
        }
    }
    let kind = call(&operation.symbol, Vec::new());
    let start = pattern.facts.len();
    lowering.match_facts(
        kind.clone(),
        MatchPorts {
            node: &node,
            operands: &operands,
            results: std::slice::from_ref(result),
        },
        checked,
        &mut pattern.facts,
    )?;
    let node = lowering.term(&node)?;
    if let Some(attribute) = &checked.literal_attribute {
        let handle = var(&format!("{label}:attribute"));
        let mut keys = vec![string("operation"), node.clone(), string(attribute)];
        let attribute_row = if policy.attributes_kind == TableKind::Function {
            ast::Fact::Eq(
                tir_egglog::span!(),
                call(&policy.attributes, keys),
                handle.clone(),
            )
        } else {
            keys.push(handle.clone());
            ast::Fact::Fact(call(&policy.attributes, keys))
        };
        pattern.facts.push(attribute_row);
        let bits = call("bigint", vec![lowering.term(&arguments[0])?]);
        pattern.facts.push(ast::Fact::Eq(
            tir_egglog::span!(),
            call(&policy.attribute_integer, vec![handle]),
            bits.clone(),
        ));
        if nested {
            let constant = call(&policy.constant, vec![kind, string(attribute), bits]);
            let candidate = call(
                &policy.definition,
                vec![node.clone(), lowering.term(result)?, constant],
            );
            pattern
                .proposals
                .push((start..pattern.facts.len(), ast::Fact::Fact(candidate)));
        }
    }
    Ok(node)
}

pub(super) fn lower(
    lowering: &RuleLowering<'_>,
    policy: &Policy,
    pattern: &Expr,
    guards: &[&Expr],
    replacement: &Expr,
) -> Result<(Vec<ast::Facts>, ast::Actions), Error> {
    let span = tir_egglog::span!();
    let result = temporary("@rewrite:result".into(), pattern);
    let mut premises = PatternFacts::default();
    let node = match_tree(lowering, policy, pattern, &result, &mut premises, false)?;
    let result = lowering.term(&result)?;
    let facts = &mut premises.facts;
    facts.push(ast::Fact::Fact(call(&policy.live, vec![node.clone()])));
    facts.push(ast::Fact::Not(call(
        &policy.pinned_operation,
        vec![node.clone()],
    )));
    for guard in guards {
        lowering.fact(lowering.predicate_row(guard)?.as_ref(), facts)?;
    }
    let actions = if let Expression::Path(path) = &replacement.value {
        if !premises.bound.contains(path) {
            return Err(Error(
                "a replacement value must come from a matched operand".into(),
            ));
        }
        facts.push(ast::Fact::Not(call(
            &policy.pinned_value,
            vec![result.clone()],
        )));
        vec![
            call(
                &policy.replace,
                vec![node.clone(), result, lowering.term(replacement)?],
            ),
            call(&policy.erase, vec![node]),
        ]
    } else {
        let (operation, arguments) = native_operation(lowering, replacement).ok_or_else(|| {
            Error("a replacement requires a matched value or declared operation".into())
        })?;
        let attribute = lowering
            .operation_matches
            .and_then(|matches| matches.get(&replacement.range.start))
            .and_then(|checked| checked.literal_attribute.as_ref())
            .ok_or_else(|| {
                Error(
                    "a constructed rewrite replacement requires a declared literal operation"
                        .into(),
                )
            })?;
        let constant = call(
            &policy.constant,
            vec![
                call(&operation.symbol, Vec::new()),
                string(attribute),
                call("bigint", vec![lowering.term(&arguments[0])?]),
            ],
        );
        vec![call(&policy.definition, vec![node, result, constant])]
    };
    Ok((
        premises.alternatives(),
        ast::GenericActions(
            actions
                .into_iter()
                .map(|expr| ast::Action::Expr(span, expr))
                .collect(),
        ),
    ))
}
