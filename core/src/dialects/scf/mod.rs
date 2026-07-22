use std::sync::Arc;

use crate::builtin::{IntegerType, TokenType};
use crate::{
    Context, Error, LoopLike, Operation, RegionGuard, Terminator, TokenScope, TypeId, ValueId,
    dialect, operation,
};

use crate as tir;
use crate::Any as AnyConstraint;
use crate::Value;
use crate::parse::common::Cursor;

pub mod ops {
    pub use super::{
        BreakOp, ConditionOp, ContinueOp, ForOp, IfOp, WhileOp, YieldOp, r#break, condition,
        r#continue, r#for, r#if, r#while, r#yield,
    };
}

dialect! {
    ScfDialect {
        name: "scf",
        operations: [
            ForOp,
            WhileOp,
            IfOp,
            ConditionOp,
            BreakOp,
            ContinueOp,
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
        interfaces: [LoopLike, TokenScope],
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
    fn iter_arg(&self) -> ValueId {
        carried_argument(&self.loop_context(), &self.body_block()).id()
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
        carried_argument(&self.loop_context(), &self.body_block()).id()
    }
    fn latched(&self) -> ValueId {
        latched_value(self)
    }
}

impl TokenScope for ForOp {
    fn token_scope_regions(&self) -> Vec<tir::RegionId> {
        vec![self.0.regions[0]]
    }
}

impl tir::Verifiable for ForOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_single_block_region_has_terminator(context, self.body(), "scf.for body")?;
        verify_loop_carried(context, self)
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
        print_scope(fmt, &context, self.body_block())?;
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
        let scope = parse_scope(parser, context)?;
        let carried = parse_iter_args(parser, context)?;
        let body_carried = carried.as_ref().map(|carried| carried.acc.clone());
        let body = parse_loop_body(parser, context, scope, body_carried)?;

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
            init: "?AnyConstraint",
        },
        results: R {
            result: "?AnyConstraint",
        },
        regions: R {
            condition_region: Region {
                single_block: true,
            },
            body: Region {
                single_block: true,
            }
        },
        interfaces: [LoopLike, TokenScope],
    }
}

impl LoopOp for WhileOp {
    fn result(&self) -> Option<ValueId> {
        self.0.results.first().copied()
    }
    fn init_operand(&self) -> Option<ValueId> {
        self.operands().first().copied()
    }
    fn body_block(&self) -> Arc<tir::Block> {
        self.body()
    }
    fn iter_arg(&self) -> ValueId {
        self.condition_region().arguments()[0].id()
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
        carried_argument(&self.loop_context(), &self.body_block()).id()
    }
    fn latched(&self) -> ValueId {
        latched_value(self)
    }
}

impl TokenScope for WhileOp {
    fn token_scope_regions(&self) -> Vec<tir::RegionId> {
        vec![self.0.regions[1]]
    }
}

impl tir::Verifiable for WhileOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_single_block_region_has_terminator(
            context,
            self.condition_region(),
            "scf.while condition",
        )?;
        verify_single_block_region_has_terminator(context, self.body(), "scf.while body")?;
        verify_while_condition(context, self)?;
        verify_loop_carried(context, self)
    }
}

impl WhileOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        print_result_prefix(fmt, self)?;
        fmt.write("scf.while")?;
        print_loop_tail(fmt, &context, self)?;
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" do")?;
        print_scope(fmt, &context, self.body_block())?;
        if !self.0.results.is_empty() {
            fmt.write(format!(
                "(%{})",
                carried_argument(&context, &self.body_block()).id().number()
            ))?;
        }
        tir::region_format::print_op_region(fmt, &context, self, 1)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let carried = parse_iter_args(parser, context)?;
        let condition_args = carried.iter().map(|c| c.acc.clone()).collect();
        let condition_region = parser
            .parse_region_with_entry_args(context, condition_args)?
            .id();
        expect_token(parser, "do")?;
        let scope = parse_scope(parser, context)?;
        let body_carried = parse_body_carried(parser, context, &carried)?;
        let body = parse_loop_body(parser, context, scope, body_carried)?;

        let mut builder = WhileOpBuilder::new(context)
            .condition_region(condition_region)
            .body(body);
        if let Some(carried) = &carried {
            builder = builder.init(carried.init).result_type(carried.ty);
        }
        Ok(Box::new(builder.build()))
    }
}

operation! {
    ConditionOp {
        name: "condition",
        dialect: "scf",
        operands: O {
            condition: "crate::Integer<1>",
            value: "?AnyConstraint",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ConditionOp {}

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
    BreakOp {
        name: "break",
        dialect: "scf",
        operands: O {
            scope: "crate::builtin::TokenType",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for BreakOp {}

operation! {
    ContinueOp {
        name: "continue",
        dialect: "scf",
        operands: O {
            scope: "crate::builtin::TokenType",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ContinueOp {}

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
    fn iter_arg(&self) -> ValueId;
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
    let acc = op.iter_arg();
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

fn print_scope(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    body: Arc<tir::Block>,
) -> Result<(), std::fmt::Error> {
    if let Some(scope) = body
        .arguments()
        .iter()
        .find(|argument| argument.ty() == TokenType::new(context))
    {
        fmt.write(format!(" scope(%{})", scope.id().number()))?;
    }
    Ok(())
}

fn parse_scope(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Option<Value>, (tir::parse::Span, Error)> {
    if !parser.parse_token("scope") {
        return Ok(None);
    }
    expect_token(parser, "(")?;
    let name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
        .to_string();
    expect_token(parser, ")")?;
    let scope = context.create_value(TokenType::new(context), None);
    parser.define_value(&name, scope.id());
    Ok(Some(scope))
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
    scope: Option<Value>,
    carried: Option<Value>,
) -> Result<tir::RegionId, (tir::parse::Span, Error)> {
    let entry_args = scope.into_iter().chain(carried).collect();
    Ok(parser
        .parse_region_with_entry_args(context, entry_args)?
        .id())
}

fn parse_body_carried(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
    carried: &Option<Carried>,
) -> Result<Option<Value>, (tir::parse::Span, Error)> {
    let Some(carried) = carried else {
        return Ok(None);
    };
    expect_token(parser, "(")?;
    let name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
        .to_string();
    expect_token(parser, ")")?;
    let value = context.create_value(carried.ty, None);
    parser.define_value(&name, value.id());
    Ok(Some(value))
}

fn carried_argument(context: &Context, body: &Arc<tir::Block>) -> Value {
    body.arguments()
        .iter()
        .find(|argument| argument.ty() != TokenType::new(context))
        .cloned()
        .expect("value-producing loop has a carried argument")
}

/// Verify a loop's carried value: a result requires a matching init operand, a single
/// carried body argument, and a matching yielded value; a resultless loop has none.
fn verify_loop_carried<T: LoopOp + Operation>(context: &Context, op: &T) -> Result<(), Error> {
    let label = format!("{}.{}", T::dialect(), T::name());
    let body = op.body_block();
    let terminator = context.get_op(*body.op_ids().last().unwrap());
    let yielded = terminator
        .is::<YieldOp>()
        .then(|| terminator.operands.first().copied())
        .flatten();
    let token = TokenType::new(context);
    let scope_args = body
        .arguments()
        .iter()
        .filter(|argument| argument.ty() == token)
        .collect::<Vec<_>>();
    let body_args = body
        .arguments()
        .iter()
        .filter(|argument| argument.ty() != token)
        .collect::<Vec<_>>();
    if scope_args.len() > 1 {
        return Err(Error::VerificationError(format!(
            "{label} body must have at most one token scope"
        )));
    }
    if !terminator.is::<YieldOp>() && !terminator.is::<BreakOp>() && !terminator.is::<ContinueOp>()
    {
        return Err(Error::VerificationError(format!(
            "{label} body must end with scf.yield, scf.break, or scf.continue"
        )));
    }
    if (terminator.is::<BreakOp>() || terminator.is::<ContinueOp>())
        && (scope_args.len() != 1 || terminator.operands != [scope_args[0].id()])
    {
        return Err(Error::VerificationError(format!(
            "{label} exit must consume its body token scope"
        )));
    }

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
            if !terminator.is::<YieldOp>() || yielded.map(|v| context.get_value(v).ty()) != Some(ty)
            {
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

fn verify_while_condition(context: &Context, op: &WhileOp) -> Result<(), Error> {
    let block = op.condition_region();
    let terminator = context.get_op(*block.op_ids().last().unwrap());
    if !terminator.is::<ConditionOp>() {
        return Err(Error::VerificationError(
            "scf.while condition must end with scf.condition".to_string(),
        ));
    }
    let arguments = block.arguments();
    match op.0.results.first().copied() {
        Some(result) => {
            let ty = context.get_value(result).ty();
            if arguments.len() != 1 || arguments[0].ty() != ty {
                return Err(Error::VerificationError(
                    "scf.while condition must carry the result type".to_string(),
                ));
            }
            if terminator.operands.len() != 2
                || context.get_value(terminator.operands[1]).ty() != ty
            {
                return Err(Error::VerificationError(
                    "scf.condition must forward the carried value".to_string(),
                ));
            }
        }
        None => {
            if !arguments.is_empty() || terminator.operands.len() != 1 {
                return Err(Error::VerificationError(
                    "resultless scf.while must not carry a condition value".to_string(),
                ));
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
