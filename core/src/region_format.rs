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

    let (args, deps) = (block.value_arguments(), block.dep_arguments());
    if !args.is_empty() || !deps.is_empty() {
        fmt.write("(")?;
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                fmt.write(", ")?;
            }
            fmt.write(format!("%{}: ", arg.id().number()))?;
            context.print_type(arg.ty(), fmt)?;
        }
        let deps: Vec<_> = deps.iter().map(crate::Value::id).collect();
        crate::dependency::print_dep_list(fmt, &deps, !args.is_empty())?;
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
    fmt.writeln(" {")?;
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
    let ops =
        crate::region::topological_order(context, region.id()).unwrap_or_else(|_| region.op_ids());
    fmt.writeln(" {")?;
    fmt.push();
    for op in ops {
        context.get_op(op).as_dyn_op().print(fmt)?;
    }
    fmt.write("->")?;
    let values = region.value_results();
    if !values.is_empty() {
        fmt.write(" ")?;
        crate::dependency::print_value_list(fmt, &values)?;
    }
    crate::dependency::print_dep_list(fmt, &region.dep_results(), true)?;
    fmt.writeln("")?;
    fmt.pop();
    fmt.writeln("}")?;
    Ok(())
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
