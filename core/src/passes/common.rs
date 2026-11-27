//! Shared IR mutation, edit validation, and DataLayout query helpers reused across
//! declarative CDSL passes (`eqsat`, `memopt`, `control-simplify`, `dce`).

use tir_egglog::{NativeIdentity, sort::BigInt};

use crate::{
    Context, OpHandle, OpId, Operation, OperationRef, PassError, RegionId, ValueId,
    attributes::AttributeValue,
    builtin::{IntegerType, ops},
    utils::APInt,
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UseSite {
    Operand(OpId, usize),
    Yield(RegionId, usize),
}

pub(crate) enum Replacement {
    Available(ValueId),
    Integer(ValueId, AttributeValue),
}

pub(crate) type CheckedEdit = (UseSite, Replacement);
pub(crate) type Alias = (UseSite, ValueId, ValueId);
pub(crate) type Local = (UseSite, ValueId, BigInt);

pub(crate) fn rule_error(error: impl std::fmt::Display) -> PassError {
    PassError::InvalidRuleSet(error.to_string())
}

pub(crate) fn placed_op(context: &Context, id: OpId) -> Option<OpHandle> {
    (context.has_operation(id)
        && (context.parent_block(id).is_some() || context.parent_nodes_region(id).is_some()))
    .then(|| context.get_op(id))
}

pub(crate) fn erase_operations(
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

pub(crate) fn literal(
    context: &Context,
    value: ValueId,
    bits: BigInt,
) -> Result<AttributeValue, PassError> {
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

pub(crate) fn check_uses(
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

pub(crate) fn check_removals(
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

pub(crate) fn apply_uses(context: &Context, edits: &[CheckedEdit]) -> Result<(), PassError> {
    let mut yields = Vec::new();
    for (site, replacement) in edits {
        let value = match replacement {
            Replacement::Available(value) => *value,
            Replacement::Integer(expected, value) => {
                let constant =
                    ops::constant(context, value.clone(), context.get_value(*expected).ty())
                        .build();
                match *site {
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
        match *site {
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

pub(crate) fn apply_edits(
    context: &Context,
    aliases: Vec<Alias>,
    local: Vec<Local>,
    mut redirected: Vec<ValueId>,
    mut erase: Vec<OpId>,
) -> Result<(), PassError> {
    redirected.sort_unstable();
    erase.sort_unstable();
    let checked = check_uses(context, aliases, local, &erase)?;
    let removals = check_removals(context, &redirected, &erase, &checked)?;
    apply_uses(context, &checked)?;
    erase_operations(context, removals)
}

/// Resolved DataLayout answers for a batch of `(OpId, OperationKind)` requests.
#[derive(Default)]
pub(crate) struct LayoutAnswers {
    pub pointer_sizes: Vec<(OpId, i64)>,
    pub store_layouts: Vec<(OpId, BigInt, BigInt)>,
    pub scoped_ptradds: Vec<OpId>,
    pub const_values: Vec<(OpId, BigInt)>,
}

pub(crate) fn resolve_layout_queries(
    context: &Context,
    requests: &[(OpId, NativeIdentity)],
) -> Result<LayoutAnswers, PassError> {
    let mut out = LayoutAnswers::default();
    for &(id, kind) in requests {
        if context.operation_kind(id) != Some(kind) {
            return Err(PassError::InvalidRuleSet(
                "layout query has the wrong operation declaration".into(),
            ));
        }
        let Some(layout) = crate::layout::DataLayout::for_op(context, id) else {
            continue;
        };
        let op = context.get_op(id);
        if op.is::<crate::ptr::ops::StoreOp>()
            || op.is::<crate::ptr::ops::LoadOp>()
            || op.is::<crate::ptr::ops::PtrAddOp>()
        {
            if let Some(width) = layout.pointer_size().filter(|&width| width != 0) {
                out.pointer_sizes.push((id, i64::from(width)));
            }
            if op.is::<crate::ptr::ops::StoreOp>()
                && let Some(data) = op.operands().first()
                && let Some(access) = layout.layout_of(context.get_value(*data).ty())
                && let Ok(size) = access.size.to_string().parse::<BigInt>()
                && let Ok(align) = access.align.to_string().parse::<BigInt>()
            {
                out.store_layouts.push((id, size, align));
            }
            if op.is::<crate::ptr::ops::PtrAddOp>()
                && let Some(offset) = op.operands().get(1)
                && let Some(ty) = IntegerType::view(context, context.get_value(*offset).ty())
                && layout.index_width().map(u64::from) == Some(ty.width())
            {
                out.scoped_ptradds.push(id);
            }
            continue;
        }
        let Some(AttributeValue::Type(ty)) = op.attr("ty") else {
            continue;
        };
        let answer = if op.is::<crate::layout::ops::SizeOfOp>() {
            layout.layout_of(ty).map(|value| value.size)
        } else if op.is::<crate::layout::ops::AlignOfOp>() {
            layout.layout_of(ty).map(|value| value.align)
        } else if op.is::<crate::layout::ops::OffsetOfOp>() {
            op.attr("field")
                .and_then(|field| layout.offset_in_bits(ty, field.as_str()?))
        } else {
            None
        };
        if let Some(bits) = answer.and_then(|value| value.to_string().parse::<BigInt>().ok()) {
            out.const_values.push((id, bits));
        }
    }
    Ok(out)
}
