use crate::Any;
use crate::attributes::AttributeValue;
use crate::{
    BlockId, BranchGuard, BranchTerminator, Context, Error, Operation, Terminator, ValueId,
    dialect, operation,
};

use crate as tir;

pub mod ops {
    pub use super::{br, cond_br};
}

dialect! {
    CfgDialect {
        name: "cfg",
        operations: [BranchOp, CondBranchOp],
        types: [],
    }
}

operation! {
    BranchOp {
        name: "br",
        dialect: "cfg",
        format: "custom",
        operands: O {
            dest_args: "*Any",
        },
        attributes: A {
            dest: "Block",
        },
        interfaces: [Terminator, BranchTerminator],
    }
}

impl Terminator for BranchOp {
    fn successors(&self) -> Vec<BlockId> {
        vec![self.dest()]
    }
}

impl BranchTerminator for BranchOp {
    fn successor_operands(&self) -> Vec<(BlockId, Vec<ValueId>)> {
        vec![(self.dest(), self.dest_args())]
    }
}

impl BranchOp {
    pub fn dest(&self) -> BlockId {
        block_attr(self, "dest")
    }

    /// The values forwarded to the destination block's arguments.
    pub fn dest_args(&self) -> Vec<ValueId> {
        self.operands().to_vec()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        fmt.write("cfg.br ")?;
        print_successor(fmt, &context, self.dest(), &self.dest_args())?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let (dest, dest_args) = parse_successor(parser, context)?;
        Ok(Box::new(
            BranchOpBuilder::new(context)
                .dest_args(dest_args)
                .attr("dest", AttributeValue::Block(dest))
                .build(),
        ))
    }
}

operation! {
    CondBranchOp {
        name: "cond_br",
        dialect: "cfg",
        format: "custom",
        operands: O {
            condition: "crate::Integer<1>",
            true_args: "*Any",
            false_args: "*Any",
        },
        attributes: A {
            true_dest: "Block",
            false_dest: "Block",
        },
        interfaces: [Terminator, BranchTerminator, BranchGuard],
    }
}

impl Terminator for CondBranchOp {
    fn successors(&self) -> Vec<BlockId> {
        vec![self.true_dest(), self.false_dest()]
    }
}

impl BranchTerminator for CondBranchOp {
    fn successor_operands(&self) -> Vec<(BlockId, Vec<ValueId>)> {
        vec![
            (self.true_dest(), self.true_args()),
            (self.false_dest(), self.false_args()),
        ]
    }
}

impl BranchGuard for CondBranchOp {
    fn guarded_successors(&self) -> Vec<(BlockId, ValueId, bool)> {
        vec![
            (self.true_dest(), self.condition(), true),
            (self.false_dest(), self.condition(), false),
        ]
    }
}

impl CondBranchOp {
    pub fn condition(&self) -> ValueId {
        self.operands()[0]
    }

    pub fn true_dest(&self) -> BlockId {
        block_attr(self, "true_dest")
    }

    pub fn false_dest(&self) -> BlockId {
        block_attr(self, "false_dest")
    }

    /// The values forwarded to the true successor's block arguments.
    pub fn true_args(&self) -> Vec<ValueId> {
        let (start, end) = self.true_range();
        self.operands()[start..end].to_vec()
    }

    /// The values forwarded to the false successor's block arguments.
    pub fn false_args(&self) -> Vec<ValueId> {
        let (start, end) = self.false_range();
        self.operands()[start..end].to_vec()
    }

    // Operand layout is [condition, true_args.., false_args..]; the segment sizes
    // [1, t, f] recovered from the op tell where each successor's args sit.
    fn true_range(&self) -> (usize, usize) {
        let segs = operand_segments(self);
        let t = segs.get(1).copied().unwrap_or(0);
        (1, 1 + t)
    }

    fn false_range(&self) -> (usize, usize) {
        let segs = operand_segments(self);
        let t = segs.get(1).copied().unwrap_or(0);
        let f = segs.get(2).copied().unwrap_or(0);
        (1 + t, 1 + t + f)
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        fmt.write(format!("cfg.cond_br %{}, ", self.condition().number()))?;
        print_successor(fmt, &context, self.true_dest(), &self.true_args())?;
        fmt.write(", ")?;
        print_successor(fmt, &context, self.false_dest(), &self.false_args())?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser)?;
        expect_token(parser, ",")?;
        let (true_dest, true_args) = parse_successor(parser, context)?;
        expect_token(parser, ",")?;
        let (false_dest, false_args) = parse_successor(parser, context)?;

        Ok(Box::new(
            CondBranchOpBuilder::new(context)
                .condition(condition)
                .true_args(true_args)
                .false_args(false_args)
                .attr("true_dest", AttributeValue::Block(true_dest))
                .attr("false_dest", AttributeValue::Block(false_dest))
                .build(),
        ))
    }
}

fn block_attr(op: &impl Operation, name: &str) -> BlockId {
    match op.attr(name) {
        Some(AttributeValue::Block(id)) => id,
        _ => panic!("{name} must be a block reference"),
    }
}

fn operand_segments(op: &impl Operation) -> Vec<usize> {
    op.attr("operand_segment_sizes")
        .and_then(|value| match value {
            AttributeValue::Array(items) => Some(items),
            _ => None,
        })
        .map(|items| {
            items
                .iter()
                .map(|v| match v {
                    AttributeValue::UInt(n) => *n as usize,
                    _ => 0,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Print a successor as `^bbN` followed by an optional MLIR-style argument list
/// `(%a, %b : t1, t2)` when the branch forwards block arguments.
fn print_successor(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    block: BlockId,
    args: &[ValueId],
) -> Result<(), std::fmt::Error> {
    fmt.write(format!("^bb{}", fmt.region_block_number(block)))?;
    if args.is_empty() {
        return Ok(());
    }
    fmt.write("(")?;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{}", arg.number()))?;
    }
    fmt.write(" : ")?;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            fmt.write(", ")?;
        }
        context.print_type(context.get_value(*arg).ty(), fmt)?;
    }
    fmt.write(")")
}

fn parse_successor(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<(BlockId, Vec<ValueId>), (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let label = parser
        .parse_block_label()
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("^label")))?
        .to_string();

    let mut args = vec![];
    let mut arg_types = vec![];
    if parser.parse_token("(") {
        loop {
            args.push(parse_value_id(parser)?);
            if parser.parse_token(",") {
                continue;
            }
            break;
        }
        expect_token(parser, ":")?;
        loop {
            arg_types.push(parse_arg_type(parser, context)?);
            if parser.parse_token(",") {
                continue;
            }
            break;
        }
        expect_token(parser, ")")?;
    }

    let block = parser.resolve_region_block_label(context, &label, &arg_types)?;
    Ok((block, args))
}

fn parse_value_id(
    parser: &mut tir::parse::text::Parser,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let value_ref = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    parser
        .resolve_value(value_ref)
        .ok_or_else(|| (parser.span(), Error::UnknownValueRef(value_ref.to_string())))
}

fn parse_arg_type(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::TypeId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    parser
        .parse_type(context)?
        .ok_or_else(|| (parser.span(), Error::ExpectedType))
}

fn expect_token(
    parser: &mut tir::parse::text::Parser,
    token: &'static str,
) -> Result<(), (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    if parser.parse_token(token) {
        Ok(())
    } else {
        Err((parser.span(), Error::ExpectedToken(token)))
    }
}
