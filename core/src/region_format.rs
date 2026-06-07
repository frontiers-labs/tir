use std::sync::Arc;

use crate::{Block, Context, IRFormatter, Operation, Region};

pub fn print_block_label(
    fmt: &mut IRFormatter,
    context: &Context,
    block: &Arc<Block>,
) -> Result<(), std::fmt::Error> {
    let args = block.arguments();
    if args.is_empty() {
        fmt.writeln(format!("^bb{}:", block.id().number()))?;
        return Ok(());
    }

    fmt.write(format!("^bb{}(", block.id().number()))?;
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
    fmt.writeln(" {")?;
    fmt.push();
    let mut first = true;
    for block in region.iter(context.clone()) {
        if first {
            first = false;
        } else {
            print_block_label(fmt, context, &block)?;
        }
        for op in block.iter(context.clone()) {
            op.as_dyn_op().print(fmt)?;
        }
    }
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
