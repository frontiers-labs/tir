//! Metadata attached to the IR in nested scopes: an op reads the dict attribute
//! carried by itself or by any enclosing op, with the innermost source winning.
//!
//! Used by [`DataLayout`](crate::DataLayout) and [`TargetEnv`](crate::TargetEnv),
//! and available to any dialect that wants its own scoped metadata key.

use std::collections::BTreeMap;

use crate::attributes::AttributeValue;
use crate::{Context, OpId};

pub type AttributeDict = BTreeMap<String, AttributeValue>;

/// The `key` dict attribute in scope at `op`: the dicts carried by `op` and by
/// each enclosing op, merged with the innermost source winning. Nested dicts
/// merge key by key; every other value is replaced wholesale. `None` when no
/// scope in the chain carries one.
pub fn scoped_dict(context: &Context, op: OpId, key: &str) -> Option<AttributeDict> {
    // A detached instance — an op type probed with no IR home to ask what it is
    // — belongs to no scope, so it reads no metadata.
    if !context.has_operation(op) {
        return None;
    }
    // Innermost first, so popping walks from the outermost scope inward.
    let mut scopes = vec![];
    let mut current = Some(op);
    while let Some(id) = current {
        if let Some(dict) = own_dict(context, id, key) {
            scopes.push(dict);
        }
        current = parent_op(context, id);
    }

    let mut resolved = scopes.pop()?;
    while let Some(inner) = scopes.pop() {
        merge_into(&mut resolved, inner);
    }
    Some(resolved)
}

fn own_dict(context: &Context, op: OpId, key: &str) -> Option<AttributeDict> {
    match context.get_op(op).attr(key) {
        Some(AttributeValue::Dict(entries)) => Some((*entries).clone()),
        _ => None,
    }
}

fn parent_op(context: &Context, op: OpId) -> Option<OpId> {
    context.parent_op(op)
}

/// Rejects a malformed metadata spec.
pub(crate) fn invalid_spec(message: impl Into<String>) -> crate::Error {
    crate::Error::VerificationError(message.into())
}

/// Applies `overlay` over `base`: nested dicts merge key by key, every other
/// value is replaced.
pub(crate) fn merge_into(base: &mut AttributeDict, overlay: AttributeDict) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(AttributeValue::Dict(nested)), AttributeValue::Dict(inner)) => {
                merge_into(nested, *inner)
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}
