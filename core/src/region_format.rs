use crate::BlockHandle;
use crate::RegionHandle;
use std::collections::HashMap;

use crate::{Context, IRFormatter, Operation};

pub fn region_block_numbers(
    region: &RegionHandle,
    context: &Context,
) -> HashMap<crate::BlockId, u32> {
    region
        .iter(context.clone())
        .enumerate()
        .map(|(index, block)| (block.id(), index as u32))
        .collect()
}

pub fn print_block_label(
    fmt: &mut IRFormatter,
    context: &Context,
    block: &BlockHandle,
    index: u32,
) -> Result<(), std::fmt::Error> {
    fmt.write(format!("^bb{index}"))?;

    let args = block.arguments();
    if !args.is_empty() {
        fmt.write("(")?;
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                fmt.write(", ")?;
            }
            fmt.write(format!("%{}: ", arg.id().number()))?;
            context.print_type(arg.ty(), fmt)?;
        }
        fmt.write(")")?;
    }

    let attrs = block.attributes();
    if !attrs.is_empty() {
        fmt.write(" {")?;
        for (i, attr) in attrs.iter().enumerate() {
            if i > 0 {
                fmt.write(", ")?;
            }
            fmt.write(context.resolve(attr.name))?;
            fmt.write(" = ")?;
            attr.value.print(fmt, context)?;
        }
        fmt.write("}")?;
    }

    fmt.writeln(":")?;
    Ok(())
}

pub fn print_region(
    fmt: &mut IRFormatter,
    context: &Context,
    region: &RegionHandle,
) -> Result<(), std::fmt::Error> {
    if region.is_nodes() {
        return print_nodes_region(fmt, context, region);
    }
    let numbers = region_block_numbers(region, context);
    fmt.push_region_block_numbers(numbers);
    fmt.writeln(open_brace(fmt))?;
    fmt.push();
    for (index, block) in region.iter(context.clone()).enumerate() {
        // The entry block is implicit, so its label appears only when needed to
        // carry attributes.
        if index > 0 || !block.attributes().is_empty() {
            print_block_label(fmt, context, &block, index as u32)?;
        }
        for op in block.iter(context.clone()) {
            op.as_dyn_op().print(fmt)?;
        }
    }
    fmt.pop();
    fmt.writeln("}")?;
    fmt.pop_region_block_numbers();
    Ok(())
}

/// An unordered region prints in the evaluation order its dependencies impose,
/// then names the values it produces on one trailing `->` line. A cycle has no
/// such order; the verifier reports it, and printing falls back to insertion
/// order so a broken region can still be read.
fn print_nodes_region(
    fmt: &mut IRFormatter,
    context: &Context,
    region: &RegionHandle,
) -> Result<(), std::fmt::Error> {
    print_nodes_region_with(fmt, context, region, &[], &region.results())
}

/// [`print_region`] for an unordered region whose owner spells part of it
/// itself: `hidden` operations are left out and the `->` line names `results`
/// in place of the region's own. A counted loop prints this way, its parser
/// putting the pinned comparison and increment back.
pub fn print_nodes_region_with(
    fmt: &mut IRFormatter,
    context: &Context,
    region: &RegionHandle,
    hidden: &[crate::OpId],
    results: &[crate::ValueId],
) -> Result<(), std::fmt::Error> {
    let ops = match fmt.shuffle() {
        Some(rng) => crate::region::shuffled_topological_order(context, region.id(), rng),
        None => crate::region::topological_order(context, region.id()),
    }
    .unwrap_or_else(|_| region.op_ids());
    fmt.writeln(open_brace(fmt))?;
    fmt.push();
    for op in ops {
        if !hidden.contains(&op) {
            context.get_op(op).as_dyn_op().print(fmt)?;
        }
    }
    fmt.write("->")?;
    if !results.is_empty() {
        fmt.write(" ")?;
        print_value_group(fmt, context, results)?;
    }
    fmt.writeln("")?;
    fmt.pop();
    fmt.writeln("}")?;
    Ok(())
}

/// A region opening at the start of a line (a gamma's second arm) takes no
/// leading space; one continuing an op line does.
fn open_brace(fmt: &IRFormatter) -> &'static str {
    if fmt.at_line_start() { "{" } else { " {" }
}

/// Print `%a, %b`.
pub fn print_value_list(
    fmt: &mut IRFormatter<'_>,
    values: &[crate::ValueId],
) -> Result<(), std::fmt::Error> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{}", value.number()))?;
    }
    Ok(())
}

/// Print `%a, %b, %c, %d`: the values of `ids` that are not states, then
/// the states. A region's `->` line is spelled this way; the op reading it
/// puts the states back where its binding wants them.
pub fn print_value_group(
    fmt: &mut IRFormatter<'_>,
    context: &Context,
    ids: &[crate::ValueId],
) -> Result<(), std::fmt::Error> {
    let mut ordered = context.values_among(ids);
    ordered.extend(context.states_among(ids));
    print_value_list(fmt, &ordered)
}

/// Print `%a, %b = ` for an op that produces anything, and nothing for one
/// that does not.
pub fn print_result_prefix(
    fmt: &mut IRFormatter<'_>,
    op: &crate::OpHandle,
) -> Result<(), std::fmt::Error> {
    let results = op.results();
    if results.is_empty() {
        return Ok(());
    }
    print_value_list(fmt, &results)?;
    fmt.write(" = ")
}

/// Print ` state(%c, %d)` for an op observing any state, and nothing otherwise.
pub fn print_state_operands(
    fmt: &mut IRFormatter<'_>,
    op: &crate::OpHandle,
) -> Result<(), std::fmt::Error> {
    let states = op.state_operands();
    if states.is_empty() {
        return Ok(());
    }
    fmt.write(" state(")?;
    print_value_list(fmt, &states)?;
    fmt.write(")")
}

/// Print an op in the generic form — `%r, %s = dialect.op %a, %b state(%c) {attrs} : ty`
/// followed by its region, if it holds one — which its handle decides in full.
pub fn print_generic(
    fmt: &mut IRFormatter,
    op: &crate::OpHandle,
    name: &str,
) -> Result<(), std::fmt::Error> {
    let context = op.context.upgrade();
    print_result_prefix(fmt, op)?;
    fmt.write(name)?;
    let operands = op.value_operands();
    if !operands.is_empty() {
        fmt.write(" ")?;
        print_value_list(fmt, &operands)?;
    }
    print_state_operands(fmt, op)?;
    // `operand_segment_sizes` is bookkeeping the operand groups already spell:
    // the generic parser recomputes it from the groups it reads back.
    let segments = context.sym("operand_segment_sizes");
    let attributes: Vec<_> = op
        .attributes()
        .into_iter()
        .filter(|attribute| Some(attribute.name) != segments)
        .collect();
    if !attributes.is_empty() {
        fmt.write(" {")?;
        for (index, attribute) in attributes.iter().enumerate() {
            if index > 0 {
                fmt.write(", ")?;
            }
            fmt.write(context.resolve(attribute.name))?;
            fmt.write(" = ")?;
            attribute.value.print(fmt, &context)?;
        }
        fmt.write("}")?;
    }
    let results = op.value_results();
    if let Some(&result) = results.first() {
        fmt.write(" : ")?;
        context.print_type(context.get_value(result).ty(), fmt)?;
    }
    let regions = op.regions();
    match regions.first() {
        Some(&region) => print_region(fmt, &context, &context.get_region(region)),
        None => fmt.write("\n"),
    }
}

pub fn print_op_region(
    fmt: &mut IRFormatter,
    context: &Context,
    op: &impl Operation,
    index: usize,
) -> Result<(), std::fmt::Error> {
    let region = op.regions().nth(index).unwrap();
    print_region(fmt, context, &region)
}
