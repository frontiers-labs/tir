use std::collections::HashMap;
use std::sync::Arc;

use crate::{Block, Context, IRFormatter, Operation, Region};

pub fn region_block_numbers(
    region: &Arc<Region>,
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
    block: &Arc<Block>,
    index: u32,
) -> Result<(), std::fmt::Error> {
    let args = block.arguments();
    if args.is_empty() {
        fmt.writeln(format!("^bb{index}:"))?;
        return Ok(());
    }

    fmt.write(format!("^bb{index}("))?;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{}: ", arg.id().number()))?;
        context.print_type(arg.ty(), fmt)?;
    }
    fmt.writeln("):")?;
    Ok(())
}

pub fn print_region(
    fmt: &mut IRFormatter,
    context: &Context,
    region: &Arc<Region>,
) -> Result<(), std::fmt::Error> {
    let numbers = region_block_numbers(region, context);
    fmt.push_region_block_numbers(numbers);
    fmt.writeln(" {")?;
    fmt.push();
    for (index, block) in region.iter(context.clone()).enumerate() {
        if index > 0 {
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

pub fn print_op_region(
    fmt: &mut IRFormatter,
    context: &Context,
    op: &impl Operation,
    index: usize,
) -> Result<(), std::fmt::Error> {
    let region = op.regions().nth(index).unwrap();
    print_region(fmt, context, &region)
}
