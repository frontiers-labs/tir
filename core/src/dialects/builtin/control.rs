use crate::Any;
use crate::attributes::AttributeValue;
use crate::{BlockId, Context, Error, Operation, Terminator, ValueId, operation};

use crate as tir;

operation! {
    BranchOp {
        name: "br",
        dialect: "builtin",
        format: "custom",
        operands: O {
            dest_args: "*Any",
        },
        attributes: A {
            dest: "Block",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for BranchOp {
    fn successors(&self) -> Vec<BlockId> {
        vec![self.dest()]
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
        fmt.write("br ")?;
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
        dialect: "builtin",
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
        interfaces: [Terminator],
    }
}

impl Terminator for CondBranchOp {
    fn successors(&self) -> Vec<BlockId> {
        vec![self.true_dest(), self.false_dest()]
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
        fmt.write(format!("cond_br %{}, ", self.condition().number()))?;
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
    op.attributes()
        .iter()
        .find(|a| a.name == name)
        .and_then(|a| match a.value {
            AttributeValue::Block(id) => Some(id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{} must be a block reference", name))
}

fn operand_segments(op: &impl Operation) -> Vec<usize> {
    op.attributes()
        .iter()
        .find(|a| a.name == "operand_segment_sizes")
        .and_then(|a| match &a.value {
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
    let index = parser
        .parse_block_index()
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("^bb")))?;

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

    let block = parser.resolve_region_block_index(context, index, &arg_types)?;
    Ok((block, args))
}

fn parse_value_id(
    parser: &mut tir::parse::text::Parser,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let value_ref = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    let id = value_ref
        .parse::<u32>()
        .map_err(|_| (parser.span(), Error::UnknownValueRef(value_ref.to_string())))?;
    Ok(ValueId::from_number(id))
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

#[cfg(test)]
mod tests {
    use crate::{
        Context, IRBuilder, IRFormatter, Operation, Terminator,
        builtin::{IntegerType, UnitType, ops},
        parse::ir::parse_ir,
    };

    use super::{BranchOp, CondBranchOp};

    #[test]
    fn br_no_args_roundtrip() {
        let context = Context::with_default_dialects();
        let dest = context.create_block(vec![]);

        let op = ops::br(&context, vec![], dest.id()).build();
        assert_eq!(op.dest(), dest.id());
        assert!(op.dest_args().is_empty());

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        op.print(&mut fmt).expect("print ok");
        assert_eq!(buf.trim_end(), format!("br ^bb{}", dest.id().number()));

        let parsed = parse_ir::<BranchOp>(&context, &buf).expect("parse br");
        assert_eq!(parsed.dest(), dest.id());
    }

    #[test]
    fn br_with_args_roundtrip() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let a = context.create_value(i32_ty, None);
        let b = context.create_value(i32_ty, None);
        let (a_id, b_id) = (a.id(), b.id());
        let _block = context.create_block(vec![a, b]);
        let dest = context.create_block(vec![]);

        let op = ops::br(&context, vec![a_id, b_id], dest.id()).build();
        assert_eq!(op.dest_args(), vec![a_id, b_id]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        op.print(&mut fmt).expect("print ok");
        assert_eq!(
            buf.trim_end(),
            format!(
                "br ^bb{}(%{}, %{} : !i32, !i32)",
                dest.id().number(),
                a_id.number(),
                b_id.number()
            )
        );

        let parsed = parse_ir::<BranchOp>(&context, &buf).expect("parse br");
        assert_eq!(parsed.dest_args(), vec![a_id, b_id]);
        assert!(parsed.verify(&context).is_ok());
    }

    #[test]
    fn cond_br_with_args_roundtrip() {
        let context = Context::with_default_dialects();
        let i1_ty = IntegerType::new(&context, 1);
        let i32_ty = IntegerType::new(&context, 32);
        let cond = context.create_value(i1_ty, None);
        let a = context.create_value(i32_ty, None);
        let b = context.create_value(i32_ty, None);
        let (cond_id, a_id, b_id) = (cond.id(), a.id(), b.id());
        let _block = context.create_block(vec![cond, a, b]);
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);

        let op = ops::cond_br(&context, cond_id, vec![a_id], vec![b_id], t.id(), f.id()).build();
        assert_eq!(op.condition(), cond_id);
        assert_eq!(op.true_args(), vec![a_id]);
        assert_eq!(op.false_args(), vec![b_id]);
        assert_eq!(op.successors(), vec![t.id(), f.id()]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        op.print(&mut fmt).expect("print ok");
        assert_eq!(
            buf.trim_end(),
            format!(
                "cond_br %{}, ^bb{}(%{} : !i32), ^bb{}(%{} : !i32)",
                cond_id.number(),
                t.id().number(),
                a_id.number(),
                f.id().number(),
                b_id.number()
            )
        );

        let parsed = parse_ir::<CondBranchOp>(&context, &buf).expect("parse cond_br");
        assert_eq!(parsed.true_dest(), t.id());
        assert_eq!(parsed.false_dest(), f.id());
        assert_eq!(parsed.true_args(), vec![a_id]);
        assert_eq!(parsed.false_args(), vec![b_id]);
        assert!(parsed.verify(&context).is_ok());
    }

    #[test]
    fn cond_br_asymmetric_args() {
        let context = Context::with_default_dialects();
        let i1_ty = IntegerType::new(&context, 1);
        let i32_ty = IntegerType::new(&context, 32);
        let cond = context.create_value(i1_ty, None);
        let a = context.create_value(i32_ty, None);
        let b = context.create_value(i32_ty, None);
        let c = context.create_value(i32_ty, None);
        let (cond_id, a_id, b_id, c_id) = (cond.id(), a.id(), b.id(), c.id());
        let _block = context.create_block(vec![cond, a, b, c]);
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);

        // Two args to true, one to false: segment split must stay correct.
        let op = ops::cond_br(
            &context,
            cond_id,
            vec![a_id, b_id],
            vec![c_id],
            t.id(),
            f.id(),
        )
        .build();
        assert_eq!(op.true_args(), vec![a_id, b_id]);
        assert_eq!(op.false_args(), vec![c_id]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        op.print(&mut fmt).expect("print ok");
        let parsed = parse_ir::<CondBranchOp>(&context, &buf).expect("parse cond_br");
        assert_eq!(parsed.true_args(), vec![a_id, b_id]);
        assert_eq!(parsed.false_args(), vec![c_id]);
    }

    #[test]
    fn branches_are_terminators() {
        let context = Context::with_default_dialects();
        let dest = context.create_block(vec![]);
        let br = ops::br(&context, vec![], dest.id()).build();
        let instance = context.get_op(br.id());
        assert!(instance.as_interface::<dyn Terminator>().is_some());
    }

    #[test]
    fn cond_br_requires_i1_condition() {
        let context = Context::with_default_dialects();
        let cond = context.create_value(IntegerType::new(&context, 32), None);
        let cond_id = cond.id();
        let _block = context.create_block(vec![cond]);
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);

        let op = ops::cond_br(&context, cond_id, vec![], vec![], t.id(), f.id()).build();
        let error = op.verify(&context).expect_err("condition must be i1");
        assert!(
            error
                .to_string()
                .contains("expected constraint crate::Integer<1>")
        );
    }

    #[test]
    fn branch_terminates_function_block() {
        let context = Context::with_default_dialects();
        let region = context.create_region();
        let entry = context.create_block(vec![]);
        region.add_block(entry.id());
        let target = context.create_block(vec![]);

        let func = ops::func(&context, "jump", UnitType::new(&context), Some(region.id())).build();

        let mut builder = IRBuilder::new(func.body());
        builder.insert(ops::br(&context, vec![], target.id()).build());

        assert!(func.verify(&context).is_ok());
    }
}
