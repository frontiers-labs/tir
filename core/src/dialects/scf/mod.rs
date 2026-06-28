use std::sync::Arc;

use crate::builtin::IntegerType;
use crate::{
    Context, Error, LoopLike, Operation, RegionGuard, Terminator, TypeId, ValueId, dialect,
    operation,
};

use crate as tir;
use crate::Any as AnyConstraint;
use crate::Value;
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
            init: "?AnyConstraint",
        },
        results: R {
            result: "?AnyConstraint",
        },
        regions: R {
            body: Region {
                single_block: true,
            }
        },
        interfaces: [LoopLike],
    }
}

impl LoopOp for ForOp {
    fn result(&self) -> Option<ValueId> {
        self.0.results.first().copied()
    }
    fn init_operand(&self) -> Option<ValueId> {
        self.operands().get(3).copied()
    }
    fn body_block(&self) -> Arc<tir::Block> {
        self.body()
    }
    fn loop_context(&self) -> Context {
        self.0.context.upgrade()
    }
}

impl tir::LoopLike for ForOp {
    fn init(&self) -> ValueId {
        self.init_operand().unwrap()
    }
    fn carried_arg(&self) -> ValueId {
        self.body_block().arguments()[0].id()
    }
    fn latched(&self) -> ValueId {
        latched_value(self)
    }
}

impl tir::Verifiable for ForOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_single_block_region_has_terminator(context, self.body(), "scf.for body")?;
        verify_loop_carried(context, self, "scf.for")
    }
}

impl ForOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        print_result_prefix(fmt, self)?;
        fmt.write(format!(
            "scf.for %{}, %{}, %{}",
            self.operands()[0].number(),
            self.operands()[1].number(),
            self.operands()[2].number()
        ))?;
        print_loop_tail(fmt, &context, self)?;
        tir::region_format::print_op_region(fmt, &context, self, 0)
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
        let carried = parse_iter_args(parser, context)?;
        let body = parse_loop_body(parser, context, &carried)?;

        let mut builder = ForOpBuilder::new(context)
            .lower_bound(lower_bound)
            .upper_bound(upper_bound)
            .step(step)
            .body(body);
        if let Some(carried) = &carried {
            builder = builder.init(carried.init).result_type(carried.ty);
        }
        Ok(Box::new(builder.build()))
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
            init: "?AnyConstraint",
        },
        results: R {
            result: "?AnyConstraint",
        },
        regions: R {
            body: Region {
                single_block: true,
            }
        },
        interfaces: [LoopLike],
    }
}

impl LoopOp for WhileOp {
    fn result(&self) -> Option<ValueId> {
        self.0.results.first().copied()
    }
    fn init_operand(&self) -> Option<ValueId> {
        self.operands().get(1).copied()
    }
    fn body_block(&self) -> Arc<tir::Block> {
        self.body()
    }
    fn loop_context(&self) -> Context {
        self.0.context.upgrade()
    }
}

impl tir::LoopLike for WhileOp {
    fn init(&self) -> ValueId {
        self.init_operand().unwrap()
    }
    fn carried_arg(&self) -> ValueId {
        self.body_block().arguments()[0].id()
    }
    fn latched(&self) -> ValueId {
        latched_value(self)
    }
}

impl tir::Verifiable for WhileOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_i1_operand(context, self.condition(), "scf.while condition")?;
        verify_single_block_region_has_terminator(context, self.body(), "scf.while body")?;
        verify_loop_carried(context, self, "scf.while")
    }
}

impl WhileOp {
    fn condition(&self) -> ValueId {
        self.operands()[0]
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        print_result_prefix(fmt, self)?;
        fmt.write(format!("scf.while %{}", self.condition().number()))?;
        print_loop_tail(fmt, &context, self)?;
        tir::region_format::print_op_region(fmt, &context, self, 0)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser)?;
        let carried = parse_iter_args(parser, context)?;
        let body = parse_loop_body(parser, context, &carried)?;

        let mut builder = WhileOpBuilder::new(context).condition(condition).body(body);
        if let Some(carried) = &carried {
            builder = builder.init(carried.init).result_type(carried.ty);
        }
        Ok(Box::new(builder.build()))
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
        results: R {
            result: "?AnyConstraint",
        },
        regions: R {
            then_body: Region {
                single_block: true,
            },
            else_body: Region {
                single_block: true,
            }
        },
        interfaces: [RegionGuard],
    }
}

impl tir::RegionGuard for IfOp {
    fn guarded_regions(&self) -> Vec<(tir::RegionId, ValueId, bool)> {
        vec![
            (self.0.regions[0], self.condition(), true),
            (self.0.regions[1], self.condition(), false),
        ]
    }
}

impl tir::Verifiable for IfOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_i1_operand(context, self.condition(), "scf.if condition")?;
        verify_single_block_region_has_terminator(context, self.then_body(), "scf.if then body")?;
        verify_single_block_region_has_terminator(context, self.else_body(), "scf.if else body")?;

        // A value-producing `scf.if` is a γ merge: each arm must yield a value of the
        // result type; a resultless `scf.if` must yield nothing.
        let result_ty = self.0.results.first().map(|&r| context.get_value(r).ty());
        verify_region_yield(context, self.then_body(), result_ty, "scf.if then body")?;
        verify_region_yield(context, self.else_body(), result_ty, "scf.if else body")
    }
}

impl IfOp {
    fn condition(&self) -> ValueId {
        self.operands()[0]
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        if let Some(&result) = self.0.results.first() {
            fmt.write(format!("%{} = ", result.number()))?;
        }
        fmt.write(format!("scf.if %{}", self.condition().number()))?;
        if let Some(&result) = self.0.results.first() {
            fmt.write(" -> ")?;
            context.print_type(context.get_value(result).ty(), fmt)?;
        }
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" else")?;
        tir::region_format::print_op_region(fmt, &context, self, 1)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser)?;
        let result_type = parse_result_type(parser, context)?;
        let then_body = parser.parse_region(context)?.id();
        expect_token(parser, "else")?;
        let else_body = parser.parse_region(context)?.id();

        let mut builder = IfOpBuilder::new(context)
            .condition(condition)
            .then_body(then_body)
            .else_body(else_body);
        if let Some(ty) = result_type {
            builder = builder.result_type(ty);
        }
        Ok(Box::new(builder.build()))
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
    parser
        .resolve_value(value_ref)
        .ok_or_else(|| (parser.span(), Error::UnknownValueRef(value_ref.to_string())))
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

/// The per-op specifics a structured loop exposes for μ-gate construction, printing,
/// and verification: its optional result, optional init operand, and body block.
trait LoopOp: Operation {
    fn result(&self) -> Option<ValueId>;
    fn init_operand(&self) -> Option<ValueId>;
    fn body_block(&self) -> Arc<tir::Block>;
    fn loop_context(&self) -> Context;
}

/// A parsed `iter_args(%acc = %init) -> <ty>` clause: the created carried block
/// argument, its init operand, and the carried type.
struct Carried {
    acc: Value,
    init: ValueId,
    ty: TypeId,
}

/// The value a loop body yields on its back edge: the next iteration's carried value.
fn latched_value(op: &impl LoopOp) -> ValueId {
    let context = op.loop_context();
    let body = op.body_block();
    context.get_op(*body.op_ids().last().unwrap()).operands[0]
}

/// Print a `%r = ` binding for a value-producing loop, nothing for a side-effecting one.
fn print_result_prefix(
    fmt: &mut tir::IRFormatter,
    op: &impl LoopOp,
) -> Result<(), std::fmt::Error> {
    if let Some(result) = op.result() {
        fmt.write(format!("%{} = ", result.number()))?;
    }
    Ok(())
}

/// Print the `iter_args(%acc = %init) -> <ty>` clause of a value-producing loop.
fn print_loop_tail(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    op: &impl LoopOp,
) -> Result<(), std::fmt::Error> {
    let Some(result) = op.result() else {
        return Ok(());
    };
    let acc = op.body_block().arguments()[0].id();
    let init = op
        .init_operand()
        .expect("value-producing loop has an init operand");
    fmt.write(format!(
        " iter_args(%{} = %{}) -> ",
        acc.number(),
        init.number()
    ))?;
    context.print_type(context.get_value(result).ty(), fmt)
}

/// Parse an optional `iter_args(%acc = %init) -> <ty>` clause, creating the carried
/// block argument bound to `%acc`.
fn parse_iter_args(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Option<Carried>, (tir::parse::Span, Error)> {
    if !parser.parse_token("iter_args") {
        return Ok(None);
    }
    expect_token(parser, "(")?;
    let acc_name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
        .to_string();
    expect_token(parser, "=")?;
    let init = parse_value_id(parser)?;
    expect_token(parser, ")")?;
    let ty =
        parse_result_type(parser, context)?.ok_or_else(|| (parser.span(), Error::ExpectedType))?;
    let acc = context.create_value(ty, None);
    parser.define_value(&acc_name, acc.id());
    Ok(Some(Carried { acc, init, ty }))
}

/// Parse a loop body, seeding its entry block with the carried argument when present.
fn parse_loop_body(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
    carried: &Option<Carried>,
) -> Result<tir::RegionId, (tir::parse::Span, Error)> {
    let entry_args = carried.iter().map(|c| c.acc.clone()).collect();
    Ok(parser
        .parse_region_with_entry_args(context, entry_args)?
        .id())
}

/// Verify a loop's carried value: a result requires a matching init operand, a single
/// carried body argument, and a matching yielded value; a resultless loop has none.
fn verify_loop_carried(context: &Context, op: &impl LoopOp, label: &str) -> Result<(), Error> {
    let body = op.body_block();
    let yielded = context
        .get_op(*body.op_ids().last().unwrap())
        .operands
        .first()
        .copied();
    let body_args = body.arguments();

    match op.result() {
        Some(result) => {
            let ty = context.get_value(result).ty();
            let init = op.init_operand().ok_or_else(|| {
                Error::VerificationError(format!("{label} with a result needs an init operand"))
            })?;
            if context.get_value(init).ty() != ty {
                return Err(Error::VerificationError(format!(
                    "{label} init type must match the result type"
                )));
            }
            if body_args.len() != 1 || body_args[0].ty() != ty {
                return Err(Error::VerificationError(format!(
                    "{label} body must carry one argument of the result type"
                )));
            }
            if yielded.map(|v| context.get_value(v).ty()) != Some(ty) {
                return Err(Error::VerificationError(format!(
                    "{label} body must yield the carried value"
                )));
            }
        }
        None => {
            if op.init_operand().is_some() || !body_args.is_empty() || yielded.is_some() {
                return Err(Error::VerificationError(format!(
                    "{label} without a result must not carry a value"
                )));
            }
        }
    }
    Ok(())
}

/// Parse an optional `-> <type>` result-type clause.
fn parse_result_type(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Option<tir::TypeId>, (tir::parse::Span, Error)> {
    if !parser.parse_token("->") {
        return Ok(None);
    }
    let ty = parser
        .parse_type(context)?
        .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
    Ok(Some(ty))
}

/// Check that a structured region's terminator yields a value of `expected` type, or
/// yields nothing when `expected` is `None`. Assumes the terminator already exists.
fn verify_region_yield(
    context: &Context,
    block: Arc<tir::Block>,
    expected: Option<tir::TypeId>,
    label: &str,
) -> Result<(), Error> {
    let terminator = context.get_op(*block.op_ids().last().unwrap());
    let operands = &terminator.operands;
    match expected {
        Some(ty) => {
            if operands.len() != 1 || context.get_value(operands[0]).ty() != ty {
                return Err(Error::VerificationError(format!(
                    "{label} must yield one value matching the result type"
                )));
            }
        }
        None => {
            if !operands.is_empty() {
                return Err(Error::VerificationError(format!(
                    "{label} must not yield a value"
                )));
            }
        }
    }
    Ok(())
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
    use crate::{IRBuilder, Operation, builtin::ops as builtin_ops};

    fn terminated_region(context: &Context) -> tir::RegionId {
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        let mut builder = IRBuilder::new(block);
        builder.insert(ops::r#yield(context, tir::Operand::none()).build());
        region.id()
    }

    // Roundtrip and verifier coverage for scf.for/if/while lives in the
    // FileCheck suite under core/checks (IRRoundtrip and Verifier).

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
            None,
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
