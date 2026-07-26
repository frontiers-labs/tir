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
    context
        .get_op(op)
        .attributes
        .iter()
        .find(|attribute| attribute.name == key)
        .and_then(|attribute| match &attribute.value {
            AttributeValue::Dict(entries) => Some(entries.clone()),
            _ => None,
        })
}

fn parent_op(context: &Context, op: OpId) -> Option<OpId> {
    let block = context.parent_block(op)?;
    let region = context.parent_region(block)?;
    context.get_region(region).parent_op()
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
                merge_into(nested, inner)
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{UnitType, ops};
    use crate::{Context, IRBuilder, Operation};

    fn dict(entries: impl IntoIterator<Item = (&'static str, AttributeValue)>) -> AttributeValue {
        AttributeValue::Dict(
            entries
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        )
    }

    #[test]
    fn nothing_is_in_scope_without_the_attribute() {
        let context = Context::with_default_dialects();
        let module = ops::module(&context, None).build();

        assert!(scoped_dict(&context, module.id(), "data_layout").is_none());
    }

    #[test]
    fn a_nested_op_reads_the_enclosing_scope() {
        let context = Context::with_default_dialects();
        let module = ops::module(&context, None)
            .attr("data_layout", dict([("endianness", "little".into())]))
            .build();
        let mut builder = IRBuilder::new(module.body());
        let nested = builder.insert(ops::module_end(&context).build());

        let resolved = scoped_dict(&context, nested.id(), "data_layout").expect("module scope");

        assert_eq!(resolved.get("endianness"), Some(&"little".into()));
    }

    #[test]
    fn an_inner_scope_overrides_one_nested_entry() {
        let context = Context::with_default_dialects();
        let module = ops::module(&context, None)
            .attr(
                "data_layout",
                dict([
                    ("endianness", "little".into()),
                    (
                        "types",
                        dict([
                            ("i32", dict([("abi", 32.into())])),
                            ("i64", dict([("abi", 64.into())])),
                        ]),
                    ),
                ]),
            )
            .build();
        let mut builder = IRBuilder::new(module.body());
        let func = builder.insert(
            ops::func(&context, "f", UnitType::new(&context), None)
                .attr(
                    "data_layout",
                    dict([("types", dict([("i32", dict([("abi", 8.into())]))]))]),
                )
                .build(),
        );

        let resolved = scoped_dict(&context, func.id(), "data_layout").expect("module scope");

        let AttributeValue::Dict(types) = &resolved["types"] else {
            panic!("types must stay a dict");
        };
        // The func overrides i32 only: i64 and the sibling endianness survive.
        assert_eq!(types["i32"], dict([("abi", 8.into())]));
        assert_eq!(types["i64"], dict([("abi", 64.into())]));
        assert_eq!(resolved.get("endianness"), Some(&"little".into()));
    }

    #[test]
    fn an_inner_scope_replaces_an_array_entry() {
        let context = Context::with_default_dialects();
        let module = ops::module(&context, None)
            .attr(
                "target_env",
                dict([("features", vec!["m".into(), "a".into()].into())]),
            )
            .build();
        let mut builder = IRBuilder::new(module.body());
        let nested = builder.insert(
            ops::module_end(&context)
                .attr("target_env", dict([("features", vec!["c".into()].into())]))
                .build(),
        );

        let resolved = scoped_dict(&context, nested.id(), "target_env").expect("module scope");

        assert_eq!(resolved["features"], vec!["c".into()].into());
    }
}
