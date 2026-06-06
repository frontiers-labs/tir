use crate::attributes::AttributeValue;
use crate::{BlockId, Error, Operation, Terminator, ValueId, operation};

use crate as tir;

operation! {
    BranchOp {
        name: "br",
        dialect: "builtin",
        format: "custom",
        attributes: A {
            dest: "Block",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for BranchOp {}

impl BranchOp {
    pub fn dest(&self) -> BlockId {
        block_attr(self, "dest")
    }

    pub fn successors(&self) -> Vec<BlockId> {
        vec![self.dest()]
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.writeln(format!("br ^bb{}", self.dest().number()))
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &tir::Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let dest = parse_block_ref(parser)?;
        Ok(Box::new(
            BranchOpBuilder::new(context)
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
        },
        attributes: A {
            true_dest: "Block",
            false_dest: "Block",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for CondBranchOp {}

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

    pub fn successors(&self) -> Vec<BlockId> {
        vec![self.true_dest(), self.false_dest()]
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.writeln(format!(
            "cond_br %{}, ^bb{}, ^bb{}",
            self.condition().number(),
            self.true_dest().number(),
            self.false_dest().number()
        ))
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &tir::Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser)?;
        expect_token(parser, ",")?;
        let true_dest = parse_block_ref(parser)?;
        expect_token(parser, ",")?;
        let false_dest = parse_block_ref(parser)?;

        Ok(Box::new(
            CondBranchOpBuilder::new(context)
                .condition(condition)
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

fn parse_block_ref(
    parser: &mut tir::parse::text::Parser,
) -> Result<BlockId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    parser
        .parse_block_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("^bb")))
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
        builtin::{IntegerType, ops},
        parse::ir::parse_ir,
    };

    use super::{BranchOp, CondBranchOp};

    #[test]
    fn br_roundtrip() {
        let context = Context::with_default_dialects();
        let dest = context.create_block(vec![]);

        let op = ops::br(&context, dest.id()).build();
        assert_eq!(op.dest(), dest.id());
        assert_eq!(op.successors(), vec![dest.id()]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        op.print(&mut fmt).expect("print ok");
        assert_eq!(buf.trim_end(), format!("br ^bb{}", dest.id().number()));

        let parsed = parse_ir::<BranchOp>(&context, &buf).expect("parse br");
        assert_eq!(parsed.dest(), dest.id());
    }

    #[test]
    fn cond_br_roundtrip() {
        let context = Context::with_default_dialects();
        let cond = context.create_value(IntegerType::new(&context, 1), None);
        let cond_id = cond.id();
        let _block = context.create_block(vec![cond]);
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);

        let op = ops::cond_br(&context, cond_id, t.id(), f.id()).build();
        assert_eq!(op.condition(), cond_id);
        assert_eq!(op.successors(), vec![t.id(), f.id()]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        op.print(&mut fmt).expect("print ok");
        assert_eq!(
            buf.trim_end(),
            format!(
                "cond_br %{}, ^bb{}, ^bb{}",
                cond_id.number(),
                t.id().number(),
                f.id().number()
            )
        );

        let parsed = parse_ir::<CondBranchOp>(&context, &buf).expect("parse cond_br");
        assert_eq!(parsed.true_dest(), t.id());
        assert_eq!(parsed.false_dest(), f.id());
    }

    #[test]
    fn branches_are_terminators() {
        let context = Context::with_default_dialects();
        let dest = context.create_block(vec![]);
        let br = ops::br(&context, dest.id()).build();
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

        let op = ops::cond_br(&context, cond_id, t.id(), f.id()).build();
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

        let func = ops::func(
            &context,
            "jump",
            crate::builtin::UnitType::new(&context),
            Some(region.id()),
        )
        .build();

        let mut builder = IRBuilder::new(func.body());
        builder.insert(ops::br(&context, target.id()).build());

        assert!(func.verify(&context).is_ok());
    }
}
