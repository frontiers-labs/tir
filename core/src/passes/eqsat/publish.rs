use tir_egglog::{NativeIdentity, sort::BigInt};

use super::generated::Form;

use crate::{
    Context, NewOp, OpHandle, OpId, Operation, OperationRef, PassError, RegionId, ValueId,
    attributes::{AttributeValue, NamedAttribute},
    builtin::{IntegerType, ops},
    schema::Argument,
    utils::APInt,
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum UseSite {
    Operand(OpId, usize),
    Yield(RegionId, usize),
}

enum Replacement {
    Available(ValueId),
    Integer(ValueId, AttributeValue),
}

type CheckedEdit = (UseSite, Replacement);
type DefinitionRow = (OpId, ValueId, Form);
type Alias = (UseSite, ValueId, ValueId);
pub(super) type Local = (UseSite, ValueId, BigInt);
type Definition = (OpHandle, ValueId, Expression);

enum Expression {
    Read(ValueId),
    Constant(NativeIdentity, &'static str, AttributeValue),
    Binary(&'static crate::schema::OpSchema, Box<[Expression; 2]>),
}

pub(super) fn publish(
    context: &Context,
    rebuild: Vec<DefinitionRow>,
    edits: Vec<Alias>,
    local: Vec<Local>,
    redirected: &[ValueId],
    erase: &[OpId],
) -> Result<(), PassError> {
    let edits = check_uses(context, edits, local, erase)?;
    let removals = check_removals(context, redirected, erase, &edits)?;
    let definitions = check_definitions(context, rebuild, redirected, &edits)?;
    apply_uses(context, edits)?;
    for (op, result, expression) in definitions {
        let old = OperationRef::new(op);
        materialize(context, &old, result, expression, Some(result))?;
    }
    erase_operations(context, removals)
}

pub(super) fn erase_operations(
    context: &Context,
    removals: Vec<OpHandle>,
) -> Result<(), PassError> {
    let mut blocks = Vec::new();
    for op in removals {
        if !context.has_operation(op.id) {
            continue;
        }
        blocks.extend(context.parent_block(op.id));
        context.erase_op(&OperationRef::new(op))?;
    }
    blocks.sort_unstable();
    blocks.dedup();
    for block in blocks {
        let block = context.get_block(block);
        block.set_ops(block.op_ids());
    }
    Ok(())
}

fn read_site(context: &Context, site: UseSite) -> Option<ValueId> {
    match site {
        UseSite::Operand(user, slot) => placed_op(context, user)?.operands().get(slot).copied(),
        UseSite::Yield(region, slot) if context.has_region(region) => {
            let region = context.get_region(region);
            region
                .is_nodes()
                .then(|| region.results().get(slot).copied())
                .flatten()
        }
        _ => None,
    }
}

fn lookup(edits: &[CheckedEdit], site: UseSite) -> bool {
    edits.binary_search_by_key(&site, |edit| edit.0).is_ok()
}

fn check_uses(
    context: &Context,
    mut aliases: Vec<Alias>,
    mut local: Vec<Local>,
    erase: &[OpId],
) -> Result<Vec<CheckedEdit>, PassError> {
    aliases.sort_unstable();
    local.sort_unstable();
    local.dedup();
    let mut edits: Vec<CheckedEdit> = Vec::new();
    for (site, old, new) in aliases {
        if read_site(context, site) != Some(old)
            || !context.has_value(old)
            || !context.has_value(new)
            || context.get_value(old).ty() != context.get_value(new).ty()
        {
            return Err(PassError::InvalidRuleSet("invalid use replacement".into()));
        }
        if edits.last().is_none_or(|edit| edit.0 != site) {
            edits.push((site, Replacement::Available(new)));
        }
    }

    let aliases_end = edits.len();
    for (site, old, bits) in local {
        if lookup(&edits[..aliases_end], site)
            || matches!(site, UseSite::Operand(user, _) if erase.binary_search(&user).is_ok())
        {
            continue;
        }
        if read_site(context, site) != Some(old) || !context.has_value(old) {
            return Err(PassError::InvalidRuleSet(
                "stale scoped use replacement".into(),
            ));
        }
        if edits[aliases_end..]
            .last()
            .is_some_and(|edit| edit.0 == site)
        {
            return Err(PassError::InvalidRuleSet(
                "conflicting scoped literals".into(),
            ));
        }
        let value = literal(context, old, bits)?;
        edits.push((site, Replacement::Integer(old, value)));
    }
    edits.sort_unstable_by_key(|edit| edit.0);
    Ok(edits)
}

fn check_removals(
    context: &Context,
    redirected: &[ValueId],
    erase: &[OpId],
    edits: &[CheckedEdit],
) -> Result<Vec<OpHandle>, PassError> {
    let mut removals = Vec::new();
    for &id in erase {
        let op = placed_op(context, id).ok_or(PassError::RewriteFailed(id))?;
        let results = op.results();
        if !op.regions().is_empty()
            || results.is_empty()
            || results
                .iter()
                .any(|value| redirected.binary_search(value).is_err())
        {
            continue;
        }
        for value in results {
            let operands = context
                .uses_of(value)
                .into_iter()
                .map(|usage| UseSite::Operand(usage.op, usage.index));
            let yields = context
                .result_uses(value)
                .into_iter()
                .map(|(region, index)| UseSite::Yield(region, index));
            if !operands.chain(yields).all(|site| lookup(edits, site)) {
                return Err(PassError::RewriteFailed(id));
            }
        }
        removals.push(op);
    }
    Ok(removals)
}

fn check_definitions(
    context: &Context,
    mut forms: Vec<DefinitionRow>,
    redirected: &[ValueId],
    edits: &[CheckedEdit],
) -> Result<Vec<Definition>, PassError> {
    forms.sort_unstable_by_key(|(id, _, form)| (*id, !matches!(form, Form::Constant(..))));
    let mut definitions: Vec<Definition> = Vec::new();
    let mut custom_definition = None;
    for (id, result, form) in forms {
        let custom = matches!(form, Form::Constant(..));
        if redirected.binary_search(&result).is_ok() || (custom_definition == Some(id) && !custom) {
            continue;
        }
        let op = definition(context, id, result)?;
        let Some(expression) = decode(context, result, form, redirected)? else {
            continue;
        };
        let use_offset = edits.partition_point(|edit| edit.0 < UseSite::Operand(id, 0));
        if matches!(expression, Expression::Binary(..))
            && edits[use_offset..]
                .iter()
                .take_while(|edit| matches!(edit.0, UseSite::Operand(user, _) if id == user))
                .any(|edit| matches!(edit.1, Replacement::Integer(..)))
        {
            continue;
        }
        if definitions.last().is_some_and(|row| row.0.id == id) {
            return Err(PassError::RewriteFailed(id));
        }
        if custom {
            custom_definition = Some(id);
        }
        definitions.push((op, result, expression));
    }
    Ok(definitions)
}

fn decode(
    context: &Context,
    expected: ValueId,
    form: Form,
    redirected: &[ValueId],
) -> Result<Option<Expression>, PassError> {
    Ok(match form {
        Form::Read(id) => {
            let value = ValueId::from(id);
            (context.has_value(value)
                && redirected.binary_search(&value).is_err()
                && context.get_value(value).ty() == context.get_value(expected).ty())
            .then_some(Expression::Read(value))
        }
        Form::Bits(bits) => literal(context, expected, bits)
            .ok()
            .map(|value| Expression::Constant(ops::ConstantOp::native_identity(), "value", value)),
        Form::Bin(kind, left, right) => {
            let Some(schema) = context.schema_for_kind(kind.declaration()) else {
                return Ok(None);
            };
            let left = decode(context, expected, *left, redirected)?;
            let right = decode(context, expected, *right, redirected)?;
            match (left, right) {
                (Some(left), Some(right)) => {
                    Some(Expression::Binary(schema, Box::new([left, right])))
                }
                _ => None,
            }
        }
        Form::Constant(kind, attr, bits) => Some(custom_constant(
            context,
            expected,
            kind.declaration(),
            &attr,
            bits,
        )?),
    })
}

fn custom_constant(
    context: &Context,
    expected: ValueId,
    kind: NativeIdentity,
    attr: &str,
    bits: BigInt,
) -> Result<Expression, PassError> {
    let schema = context.schema_for_kind(kind).ok_or_else(|| {
        PassError::InvalidRuleSet(format!(
            "unregistered constant declaration '{}'",
            kind.as_str()
        ))
    })?;
    let ty = context.get_value(expected).ty();
    if !context.kind_has_interface::<dyn crate::ConstantLike>(&kind)
        || !schema.regions.is_empty()
        || schema
            .signature
            .substitution(context.terms(), &[], &[Argument::Fixed(ty)])
            .is_none()
    {
        return Err(PassError::InvalidRuleSet(format!(
            "'{}' cannot construct a constant with this result type",
            kind.as_str()
        )));
    }
    let field = schema
        .attributes
        .iter()
        .find(|field| field.name == attr && matches!(field.ty, "Int" | "?Int"))
        .ok_or_else(|| {
            PassError::InvalidRuleSet(format!(
                "'{}' has no integer attribute '{attr}'",
                kind.as_str()
            ))
        })?;
    if schema
        .attributes
        .iter()
        .any(|other| other.name != attr && !other.ty.starts_with('?'))
    {
        return Err(PassError::InvalidRuleSet(format!(
            "'{}' requires additional constant attributes",
            kind.as_str()
        )));
    }
    Ok(Expression::Constant(
        kind,
        field.name,
        literal(context, expected, bits)?,
    ))
}

fn materialize(
    context: &Context,
    anchor: &OperationRef,
    expected: ValueId,
    expression: Expression,
    root_result: Option<ValueId>,
) -> Result<ValueId, PassError> {
    let (identity, operands, attributes) = match expression {
        Expression::Read(value) if root_result.is_none() => return Ok(value),
        Expression::Read(_) => return Err(PassError::RewriteFailed(anchor.op().id)),
        Expression::Constant(kind, attr, value) => (
            kind,
            Vec::new(),
            vec![NamedAttribute::new(context.intern(attr), value)],
        ),
        Expression::Binary(schema, children) => {
            let mut operands = Vec::with_capacity(2);
            for child in *children {
                operands.push(materialize(context, anchor, expected, child, None)?);
            }
            ((schema.declaration)(), operands, Vec::new())
        }
    };
    let result = root_result.unwrap_or_else(|| {
        context
            .create_value(context.get_value(expected).ty(), None)
            .id()
    });
    let op = context.add_operation(
        crate::InsertionPoint::Root,
        NewOp::from_declaration(identity, operands, vec![result], Vec::new(), attributes),
    );
    let op = op.as_dyn_op();
    if root_result.is_some() {
        context.replace_op_keeping_results(anchor, op.as_ref())?;
    } else {
        context.insert(context.insertion_point(anchor.op().id), op.id());
    }
    Ok(result)
}

fn apply_uses(context: &Context, edits: Vec<CheckedEdit>) -> Result<(), PassError> {
    let mut yields = Vec::new();
    for (site, replacement) in edits {
        let value = match replacement {
            Replacement::Available(value) => value,
            Replacement::Integer(expected, value) => {
                let constant =
                    ops::constant(context, value, context.get_value(expected).ty()).build();
                match site {
                    UseSite::Operand(user, _) => {
                        context.insert(context.insertion_point(user), constant.id());
                    }
                    UseSite::Yield(region, _) => {
                        context.insert(crate::InsertionPoint::Region(region), constant.id())
                    }
                }
                constant.result()
            }
        };
        match site {
            UseSite::Operand(user, slot) => context.set_op_operand(user, slot, value),
            UseSite::Yield(region, slot) => yields.push((region, slot, value)),
        }
    }
    for group in yields.chunk_by(|left, right| left.0 == right.0) {
        let region = group[0].0;
        let mut results = context.get_region(region).results();
        for &(_, slot, value) in group {
            if let Some(result) = results.get_mut(slot) {
                *result = value;
            }
        }
        context.set_region_results(region, results);
    }
    Ok(())
}

fn definition(context: &Context, id: OpId, result: ValueId) -> Result<OpHandle, PassError> {
    let op = placed_op(context, id).ok_or(PassError::RewriteFailed(id))?;
    if !context.has_value(result) || op.results().as_slice() != [result] || !op.regions().is_empty()
    {
        return Err(PassError::RewriteFailed(id));
    }
    Ok(op)
}

fn placed_op(context: &Context, id: OpId) -> Option<OpHandle> {
    (context.has_operation(id)
        && (context.parent_block(id).is_some() || context.parent_nodes_region(id).is_some()))
    .then(|| context.get_op(id))
}

fn literal(context: &Context, value: ValueId, bits: BigInt) -> Result<AttributeValue, PassError> {
    let ty = context.get_value(value).ty();
    let width = IntegerType::view(context, ty)
        .ok_or_else(|| PassError::InvalidRuleSet("expected integer replacement type".into()))?
        .width();
    let spelling = bits.to_string();
    let magnitude: APInt = spelling
        .strip_prefix('-')
        .unwrap_or(&spelling)
        .parse()
        .map_err(PassError::InvalidRuleSet)?;
    let number = APInt::from_limbs(width, &magnitude.limbs());
    let number = if bits.is_negative() {
        number.neg()
    } else {
        number
    };
    Ok(AttributeValue::from_int(&number))
}
