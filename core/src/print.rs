//! Printing an operation as a standalone IR file.
//!
//! A metadata spec — a data layout, a target environment — is long enough that
//! spelling it inline buries the operation it is attached to. Printing hoists
//! every such spec into a `#name = ...` alias definition ahead of the file, the
//! form [`parse_ir`](crate::parse::ir::parse_ir) accepts, leaving the operation
//! itself carrying nothing but references.

use crate::attributes::AttributeValue;
use crate::{Context, DATA_LAYOUT, IRFormatter, OpId, Operation, TARGET_ENV};

/// Print `op` and the alias definitions of the metadata specs it carries.
pub fn print_ir(
    op: &dyn Operation,
    context: &Context,
    fmt: &mut IRFormatter,
) -> Result<(), std::fmt::Error> {
    let mut aliases = vec![];
    collect_aliases(context, op.id(), &mut aliases);

    // Definitions print before the alias table is installed, so a spec renders
    // as itself rather than as a reference to itself.
    for (name, value) in &aliases {
        fmt.write(format!("#{name} = "))?;
        value.print(fmt, context)?;
        fmt.write("\n")?;
    }
    if !aliases.is_empty() {
        fmt.write("\n")?;
    }

    fmt.set_attribute_aliases(aliases);
    op.print(fmt)
}

/// Names for the specs carried anywhere in `op`'s tree, in the order they are
/// first met. Specs that repeat share one definition.
fn collect_aliases(context: &Context, op: OpId, aliases: &mut Vec<(String, AttributeValue)>) {
    let instance = context.get_op(op);
    for attribute in instance.attributes() {
        let name = context.resolve(attribute.name);
        let aliased = matches!(name.as_str(), DATA_LAYOUT | TARGET_ENV)
            && matches!(attribute.value, AttributeValue::Dict(_));
        if !aliased || aliases.iter().any(|(_, value)| *value == attribute.value) {
            continue;
        }
        let name = unique_name(&name, aliases);
        aliases.push((name, attribute.value.clone()));
    }

    for region in instance.regions() {
        for child in context.get_region(region).op_ids() {
            collect_aliases(context, child, aliases);
        }
    }
}

/// The attribute's own name, suffixed where an op deeper in the tree overrides
/// the spec with a different one.
fn unique_name(base: &str, aliases: &[(String, AttributeValue)]) -> String {
    let taken = |candidate: &str| aliases.iter().any(|(name, _)| name == candidate);
    if !taken(base) {
        return base.to_string();
    }
    (1..)
        .map(|suffix| format!("{base}_{suffix}"))
        .find(|candidate| !taken(candidate))
        .expect("a fresh suffix always exists")
}
