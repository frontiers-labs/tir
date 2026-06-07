use std::sync::Arc;

use crate::builtin::IntegerType;
use crate::{Context, Error, Operation, Terminator, ValueId, dialect, operation};

use crate as tir;
use crate::Any as AnyConstraint;
use crate::parse::common::Cursor;

pub mod ops {
    pub use super::{ForOp, IfOp, WhileOp, YieldOp, r#for, r#if, r#while, r#yield};
}

dialect! {
    ScfDialect {
        name: "scf",
        operations: [
            ForOp,
            WhileOp,
            IfOp,
            YieldOp,
        ],
        types: [],
    }
}

operation! {
    ForOp {
        name: "for",
        dialect: "scf",
        format: "custom",
        verifier: "true",
        operands: O {
            lower_bound: "crate::builtin::IndexType",
            upper_bound: "crate::builtin::IndexType",
            step: "crate::builtin::IndexType",
        },
        regions: R {
            body: Region {
                single_block: true,
            }
        }
    }
}

impl tir::Verifiable for ForOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_single_block_region_has_terminator(context, self.body(), "scf.for body")
    }
}

impl ForOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.write(format!(
            "scf.for %{}, %{}, %{}",
            self.operands()[0].number(),
            self.operands()[1].number(),
            self.operands()[2].number()
        ))?;
        tir::region_format::print_op_region(fmt, &self.0.context.upgrade(), self, 0)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let lower_bound = parse_value_id(parser)?;
        expect_token(parser, ",")?;
        let upper_bound = parse_value_id(parser)?;
        expect_token(parser, ",")?;
        let step = parse_value_id(parser)?;
        let body = parser.parse_region(context)?.id();

        Ok(Box::new(
            ForOpBuilder::new(context)
                .lower_bound(lower_bound)
                .upper_bound(upper_bound)
                .step(step)
                .body(body)
                .build(),
        ))
    }
}

operation! {
    WhileOp {
        name: "while",
        dialect: "scf",
        format: "custom",
        verifier: "true",
        operands: O {
            condition: "crate::Integer<1>",
        },
        regions: R {
            body: Region {
                single_block: true,
            }
        }
    }
}

impl tir::Verifiable for WhileOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_i1_operand(context, self.condition(), "scf.while condition")?;
        verify_single_block_region_has_terminator(context, self.body(), "scf.while body")
    }
}

impl WhileOp {
    fn condition(&self) -> ValueId {
        self.operands()[0]
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.write(format!("scf.while %{}", self.condition().number()))?;
        tir::region_format::print_op_region(fmt, &self.0.context.upgrade(), self, 0)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser)?;
        let body = parser.parse_region(context)?.id();
        Ok(Box::new(
            WhileOpBuilder::new(context)
                .condition(condition)
                .body(body)
                .build(),
        ))
    }
}

operation! {
    IfOp {
        name: "if",
        dialect: "scf",
        format: "custom",
        verifier: "true",
        operands: O {
            condition: "crate::Integer<1>",
        },
        regions: R {
            then_body: Region {
                single_block: true,
            },
            else_body: Region {
                single_block: true,
            }
        }
    }
}

impl tir::Verifiable for IfOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_i1_operand(context, self.condition(), "scf.if condition")?;
        verify_single_block_region_has_terminator(context, self.then_body(), "scf.if then body")?;
        verify_single_block_region_has_terminator(context, self.else_body(), "scf.if else body")
    }
}

impl IfOp {
    fn condition(&self) -> ValueId {
        self.operands()[0]
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.write(format!("scf.if %{}", self.condition().number()))?;
        let context = self.0.context.upgrade();
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" else")?;
        tir::region_format::print_op_region(fmt, &context, self, 1)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser)?;
        let then_body = parser.parse_region(context)?.id();
        expect_token(parser, "else")?;
        let else_body = parser.parse_region(context)?.id();

        Ok(Box::new(
            IfOpBuilder::new(context)
                .condition(condition)
                .then_body(then_body)
                .else_body(else_body)
                .build(),
        ))
    }
}

operation! {
    YieldOp {
        name: "yield",
        dialect: "scf",
        operands: O {
            value: "?AnyConstraint",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for YieldOp {}

fn parse_value_id(
    parser: &mut tir::parse::text::Parser,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    let value_ref = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    let id = value_ref
        .parse::<u32>()
        .map_err(|_| (parser.span(), Error::UnknownValueRef(value_ref.to_string())))?;
    Ok(ValueId::from_number(id))
}

fn expect_token(
    parser: &mut tir::parse::text::Parser,
    token: &'static str,
) -> Result<(), (tir::parse::Span, Error)> {
    if parser.parse_token(token) {
        Ok(())
    } else {
        Err((parser.span(), Error::ExpectedToken(token)))
    }
}

fn verify_i1_operand(context: &Context, value: ValueId, label: &str) -> Result<(), Error> {
    let ty = context.get_value(value).ty();
    if ty != IntegerType::new(context, 1) {
        return Err(Error::VerificationError(format!(
            "{label} must have type i1"
        )));
    }
    Ok(())
}

fn verify_single_block_region_has_terminator(
    context: &Context,
    block: Arc<tir::Block>,
    label: &str,
) -> Result<(), Error> {
    if block.op_ids().is_empty() {
        return Err(Error::VerificationError(format!(
            "{label} must contain at least one operation"
        )));
    }

    let last_op = context.get_op(*block.op_ids().last().unwrap());
    if last_op.as_interface::<dyn Terminator>().is_none() {
        return Err(Error::VerificationError(format!(
            "{label} must end with a terminator"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        IRBuilder, IRFormatter, Operation,
        builtin::{IndexType, ops as builtin_ops},
        parse::ir::parse_ir,
    };

    fn terminated_region(context: &Context) -> tir::RegionId {
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        let mut builder = IRBuilder::new(block);
        builder.insert(ops::r#yield(context, tir::Operand::none()).build());
        region.id()
    }

    fn block_arg(context: &Context, ty: tir::TypeId) -> ValueId {
        let value = context.create_value(ty, None);
        let id = value.id();
        let _block = context.create_block(vec![value]);
        id
    }

    #[test]
    fn scf_for_roundtrip() {
        let context = Context::with_default_dialects();
        let index_ty = IndexType::new(&context);
        let lower = block_arg(&context, index_ty);
        let upper = block_arg(&context, index_ty);
        let step = block_arg(&context, index_ty);

        let op = ops::r#for(
            &context,
            lower,
            upper,
            step,
            Some(terminated_region(&context)),
        )
        .build();

        assert!(op.verify(&context).is_ok());

        let mut buf = String::new();
        let mut formatter = IRFormatter::new(&mut buf);
        op.print(&mut formatter).expect("print ok");
        assert!(buf.contains("scf.for"));
        assert!(buf.contains("scf.yield"));

        let parsed = parse_ir::<ForOp>(&context, &buf).expect("parse scf.for");
        assert!(parsed.verify(&context).is_ok());
    }

    #[test]
    fn scf_if_roundtrip() {
        let context = Context::with_default_dialects();
        let condition = block_arg(&context, IntegerType::new(&context, 1));

        let op = ops::r#if(
            &context,
            condition,
            Some(terminated_region(&context)),
            Some(terminated_region(&context)),
        )
        .build();

        assert!(op.verify(&context).is_ok());

        let mut buf = String::new();
        let mut formatter = IRFormatter::new(&mut buf);
        op.print(&mut formatter).expect("print ok");
        assert!(buf.contains("scf.if"));
        assert!(buf.contains("else"));

        let parsed = parse_ir::<IfOp>(&context, &buf).expect("parse scf.if");
        assert!(parsed.verify(&context).is_ok());
    }

    #[test]
    fn scf_while_roundtrip() {
        let context = Context::with_default_dialects();
        let condition = block_arg(&context, IntegerType::new(&context, 1));

        let op = ops::r#while(&context, condition, Some(terminated_region(&context))).build();

        assert!(op.verify(&context).is_ok());

        let mut buf = String::new();
        let mut formatter = IRFormatter::new(&mut buf);
        op.print(&mut formatter).expect("print ok");
        assert!(buf.contains("scf.while"));

        let parsed = parse_ir::<WhileOp>(&context, &buf).expect("parse scf.while");
        assert!(parsed.verify(&context).is_ok());
    }

    #[test]
    fn scf_if_requires_i1_condition() {
        let context = Context::with_default_dialects();
        let condition = block_arg(&context, IntegerType::new(&context, 32));

        let op = ops::r#if(
            &context,
            condition,
            Some(terminated_region(&context)),
            Some(terminated_region(&context)),
        )
        .build();

        let error = op.verify(&context).expect_err("condition must be i1");
        assert!(
            error
                .to_string()
                .contains("expected constraint crate::Integer<1>")
        );
    }

    #[test]
    fn scf_ops_nest_in_function() {
        let context = Context::with_default_dialects();
        let condition = context.create_value(IntegerType::new(&context, 1), None);
        let region = context.create_region();
        let block = context.create_block(vec![condition.clone()]);
        region.add_block(block.id());
        let func = builtin_ops::func(
            &context,
            "control",
            crate::builtin::UnitType::new(&context),
            Some(region.id()),
        )
        .build();

        let if_op = ops::r#if(
            &context,
            condition.id(),
            Some(terminated_region(&context)),
            Some(terminated_region(&context)),
        )
        .build();

        let mut builder = IRBuilder::new(func.body());
        builder.insert(if_op);
        builder.insert(builtin_ops::r#return(&context, tir::Operand::none()).build());

        assert!(func.verify(&context).is_ok());
    }
}
