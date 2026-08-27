//! Deep copies of operations and regions.
//!
//! A clone is a fresh subtree: new blocks, new block arguments, new result
//! values. References that point inside the copied subtree — operands,
//! terminator destinations — are rewritten onto the copies, while references to
//! definitions outside it are kept as they are.

use std::collections::HashMap;

use crate::{
    BlockId, Context, OpId, OpInstance, RegionId,
    attributes::{AttributeValue, NamedAttribute},
    value::ValueId,
};

#[derive(Default)]
struct Mapping {
    values: HashMap<ValueId, ValueId>,
    blocks: HashMap<BlockId, BlockId>,
}

pub(crate) fn clone_op(context: &Context, op: OpId) -> OpId {
    clone_op_into(context, op, &mut Mapping::default())
}

pub(crate) fn clone_region(context: &Context, region: RegionId) -> RegionId {
    clone_region_into(context, region, &mut Mapping::default())
}

/// Copy `region`, substituting `bindings` for the definitions outside it and
/// for its own block arguments.
///
/// A plain clone keeps outside references as they are, which is right for a copy
/// that runs where the original did. A copy that runs somewhere else — the tile
/// of a strip-mined loop, the epilogue that finishes it, the arm a versioned
/// nest picks — reads the same values under new names, and this is where those
/// names are given. A bound argument's copy is still declared, but nothing in
/// the copy names it. References the map does not mention are kept.
pub fn clone_region_with_mapping(
    context: &Context,
    region: RegionId,
    bindings: &HashMap<ValueId, ValueId>,
) -> RegionId {
    let mut mapping = Mapping {
        values: bindings.clone(),
        blocks: HashMap::new(),
    };
    clone_region_into(context, region, &mut mapping)
}

/// Blocks are created before any operation is copied, so a branch to a block
/// later in the region already has its copy to name.
fn clone_region_into(context: &Context, region: RegionId, mapping: &mut Mapping) -> RegionId {
    let clone = context.create_region();
    let source_blocks: Vec<_> = context
        .get_region(region)
        .iter(context.clone())
        .collect::<Vec<_>>();

    for block in &source_blocks {
        let arguments = block
            .arguments()
            .iter()
            .map(|argument| {
                let copy = context.create_value(argument.ty(), None);
                mapping.values.entry(argument.id()).or_insert(copy.id());
                copy
            })
            .collect();
        let copy = context.create_block(arguments);
        mapping.blocks.insert(block.id(), copy.id());
        clone.add_block(copy.id());
    }

    for block in &source_blocks {
        let target = mapping.blocks[&block.id()];
        for op in block.op_ids() {
            let copy = clone_op_into(context, op, mapping);
            context.get_block(target).append(copy);
        }
    }

    clone.id()
}

fn clone_op_into(context: &Context, op: OpId, mapping: &mut Mapping) -> OpId {
    let source = context.get_op(op);

    let regions = source
        .regions()
        .iter()
        .map(|region| clone_region_into(context, *region, mapping))
        .collect();
    let operands = source
        .operands()
        .iter()
        .map(|operand| remap_value(*operand, mapping))
        .collect();
    let results: Vec<ValueId> = source
        .results()
        .iter()
        .map(|result| {
            let copy = context.create_value(context.get_value(*result).ty(), None);
            mapping.values.insert(*result, copy.id());
            copy.id()
        })
        .collect();
    let attributes = source
        .attributes()
        .iter()
        .map(|attribute| {
            NamedAttribute::new(attribute.name, remap_attribute(&attribute.value, mapping))
        })
        .collect();

    let instance = OpInstance::new_dynamic(
        (source.dialect().as_str(), source.name().as_str()),
        context.as_context_ref(),
        operands,
        results,
        regions,
        attributes,
    );
    context.add_operation(instance).id
}

fn remap_value(value: ValueId, mapping: &Mapping) -> ValueId {
    mapping.values.get(&value).copied().unwrap_or(value)
}

fn remap_attribute(value: &AttributeValue, mapping: &Mapping) -> AttributeValue {
    match value {
        AttributeValue::Block(block) => {
            AttributeValue::Block(mapping.blocks.get(block).copied().unwrap_or(*block))
        }
        AttributeValue::Array(items) => AttributeValue::Array(
            items
                .iter()
                .map(|item| remap_attribute(item, mapping))
                .collect::<Vec<_>>()
                .into(),
        ),
        AttributeValue::Dict(entries) => AttributeValue::Dict(Box::new(
            entries
                .iter()
                .map(|(name, item)| (name.clone(), remap_attribute(item, mapping)))
                .collect(),
        )),
        other => other.clone(),
    }
}
