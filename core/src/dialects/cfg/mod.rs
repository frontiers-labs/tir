use crate::Any;
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
        let op = BranchOpBuilder::new(context)
            .dest_args(dest_args)
            .dest(dest)
            .build();
        Ok(Box::new(op))
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

    /// The values forwarded to the true successor's block arguments, the
    /// states among them last.
    pub fn true_args(&self) -> Vec<ValueId> {
        self.operands()[self.args_range(1)].to_vec()
    }

    /// The values forwarded to the false successor's block arguments, the
    /// states among them last.
    pub fn false_args(&self) -> Vec<ValueId> {
        self.operands()[self.args_range(2)].to_vec()
    }

    // Operand layout is [condition, true_args.., false_args..], one declared
    // operand group each.
    fn args_range(&self, group: usize) -> std::ops::Range<usize> {
        tir::binding::operand_segments(&self.0, 3)[group].clone()
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
        let condition = parse_value_id(parser, context)?;
        expect_token(parser, ",")?;
        let (true_dest, true_args) = parse_successor(parser, context)?;
        expect_token(parser, ",")?;
        let (false_dest, false_args) = parse_successor(parser, context)?;

        let op = CondBranchOpBuilder::new(context)
            .condition(condition)
            .true_args(true_args)
            .false_args(false_args)
            .true_dest(true_dest)
            .false_dest(false_dest)
            .build();
        Ok(Box::new(op))
    }
}

/// Print a successor as `^bbN` followed by an optional MLIR-style argument list
/// `(%a, %b : t1, t2, state(%s))` when the branch forwards block arguments.
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
    let values = context.values_among(args);
    fmt.write("(")?;
    if !values.is_empty() {
        tir::region_format::print_value_list(fmt, &values)?;
        fmt.write(" : ")?;
        for (i, arg) in values.iter().enumerate() {
            if i > 0 {
                fmt.write(", ")?;
            }
            context.print_type(context.get_value(*arg).ty(), fmt)?;
        }
    }
    tir::region_format::print_state_group(fmt, &context.states_among(args), !values.is_empty())?;
    fmt.write(")")
}

/// A parsed successor: its label and the values it is entered on, the states
/// among them last.
type Successor = (BlockId, Vec<ValueId>);

fn parse_successor(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Successor, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let label = parser
        .parse_block_label()
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("^label")))?
        .to_string();

    let mut args = vec![];
    let mut arg_types = vec![];
    if parser.parse_token("(") {
        if parser.peek_char() == Some('%') {
            loop {
                args.push(parse_value_id(parser, context)?);
                if parser.parse_token(",") {
                    continue;
                }
                break;
            }
            expect_token(parser, ":")?;
            loop {
                arg_types.push(parse_arg_type(parser, context)?);
                // The types end where the state group begins.
                let mark = parser.pos();
                if parser.parse_token(",") && parser.peek_char() == Some('!') {
                    continue;
                }
                parser.set_pos(mark);
                break;
            }
        }
        parser.parse_token(",");
        let states = parser.parse_state_operands(context)?;
        arg_types.extend(states.iter().map(|_| tir::TypeId::STATE));
        args.extend(states);
        expect_token(parser, ")")?;
    }

    let block = parser.resolve_region_block_label(context, &label, &arg_types)?;
    Ok((block, args))
}

fn parse_value_id(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let value_ref = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    Ok(parser.resolve_value(context, value_ref))
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
