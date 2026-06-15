//! The `cir` (C IR) dialect: C-specific control flow that the generic
//! `builtin`/`scf` dialects do not model.
//!
//! C loops are not counting loops: the condition is an arbitrary expression
//! re-evaluated every iteration, `do`/`while` differ in whether that test runs
//! before or after the body, and in a `for` loop `continue` runs the update
//! before the next test. Each loop is therefore a structured op carrying its
//! sub-expressions as regions:
//!
//! - [`WhileOp`]  — `cond` then `body` (pre-test).
//! - [`DoOp`]     — `body` then `cond` (post-test).
//! - [`ForOp`]    — `cond`, `body`, `step`; `continue` lands in `step`.
//!
//! A `cond` region ends with [`ConditionOp`] yielding the `i1` test; a `body`
//! or `step` region ends with [`YieldOp`] on the fall-through (back-)edge, or
//! with [`BreakOp`] / [`ContinueOp`].
//!
//! Each loop hands its `body` a single `!token` argument: the loop handle.
//! `cir.break` / `cir.continue` consume that token, so a use of one names the
//! loop it leaves and an inner loop can name an enclosing loop's handle. This
//! is the def-use edge the builtin token type was added for: the target is read
//! straight off the operand instead of recomputed from region nesting.

use tir::builtin::TokenType;
use tir::parse::common::Cursor;
use tir::parse::{Span, text::Parser};
use tir::{
    Context, Error, IRFormatter, Operation, RegionId, Terminator, Value, ValueId, dialect,
    operation,
};

pub mod ops {
    pub use super::{
        BreakOp, ConditionOp, ContinueOp, DoOp, ForOp, WhileOp, YieldOp, r#break, condition,
        r#continue, r#do, r#for, r#while, r#yield,
    };
}

dialect! {
    CirDialect {
        name: "cir",
        operations: [
            WhileOp,
            DoOp,
            ForOp,
            ConditionOp,
            YieldOp,
            BreakOp,
            ContinueOp,
        ],
        types: [],
    }
}

/// Register the `cir` dialect with a context.
pub fn register(context: &Context) {
    context.register_dialect::<CirDialect>();
}

operation! {
    WhileOp {
        name: "while",
        dialect: "cir",
        format: "custom",
        verifier: "true",
        regions: R {
            cond: Region { single_block: true },
            body: Region { single_block: true },
        }
    }
}

impl tir::Verifiable for WhileOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_cond(context, self, 0)?;
        verify_body(context, &self.body())
    }
}

impl WhileOp {
    /// The loop handle: the `!token` argument of the body's entry block.
    pub fn token(&self) -> ValueId {
        self.body().arguments()[0].id()
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        print_header(fmt, "cir.while", self.token())?;
        print_region(fmt, &context, self, "cond", 0)?;
        print_region(fmt, &context, self, "body", 1)
    }

    fn custom_parse(parser: &mut Parser, context: &Context) -> ParseResult {
        let token = parse_handle(parser, context)?;
        let cond = parse_region(parser, context, "cond")?;
        let body = parse_body(parser, context, token)?;
        Ok(Box::new(
            WhileOpBuilder::new(context).cond(cond).body(body).build(),
        ))
    }
}

operation! {
    DoOp {
        name: "do",
        dialect: "cir",
        format: "custom",
        verifier: "true",
        regions: R {
            body: Region { single_block: true },
            cond: Region { single_block: true },
        }
    }
}

impl tir::Verifiable for DoOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_body(context, &self.body())?;
        verify_cond(context, self, 1)
    }
}

impl DoOp {
    pub fn token(&self) -> ValueId {
        self.body().arguments()[0].id()
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        print_header(fmt, "cir.do", self.token())?;
        print_region(fmt, &context, self, "body", 0)?;
        print_region(fmt, &context, self, "cond", 1)
    }

    fn custom_parse(parser: &mut Parser, context: &Context) -> ParseResult {
        let token = parse_handle(parser, context)?;
        let body = parse_body(parser, context, token)?;
        let cond = parse_region(parser, context, "cond")?;
        Ok(Box::new(
            DoOpBuilder::new(context).body(body).cond(cond).build(),
        ))
    }
}

operation! {
    ForOp {
        name: "for",
        dialect: "cir",
        format: "custom",
        verifier: "true",
        regions: R {
            cond: Region { single_block: true },
            body: Region { single_block: true },
            step: Region { single_block: true },
        }
    }
}

impl tir::Verifiable for ForOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_cond(context, self, 0)?;
        verify_body(context, &self.body())
    }
}

impl ForOp {
    pub fn token(&self) -> ValueId {
        self.body().arguments()[0].id()
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        print_header(fmt, "cir.for", self.token())?;
        print_region(fmt, &context, self, "cond", 0)?;
        print_region(fmt, &context, self, "body", 1)?;
        print_region(fmt, &context, self, "step", 2)
    }

    fn custom_parse(parser: &mut Parser, context: &Context) -> ParseResult {
        let token = parse_handle(parser, context)?;
        let cond = parse_region(parser, context, "cond")?;
        let body = parse_body(parser, context, token)?;
        let step = parse_region(parser, context, "step")?;
        Ok(Box::new(
            ForOpBuilder::new(context)
                .cond(cond)
                .body(body)
                .step(step)
                .build(),
        ))
    }
}

operation! {
    ConditionOp {
        name: "condition",
        dialect: "cir",
        operands: O {
            value: "tir::Integer<1>",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ConditionOp {}

operation! {
    YieldOp {
        name: "yield",
        dialect: "cir",
        interfaces: [Terminator],
    }
}

impl Terminator for YieldOp {}

operation! {
    BreakOp {
        name: "break",
        dialect: "cir",
        operands: O {
            token: "tir::builtin::TokenType",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for BreakOp {}

operation! {
    ContinueOp {
        name: "continue",
        dialect: "cir",
        operands: O {
            token: "tir::builtin::TokenType",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ContinueOp {}

type ParseResult = Result<Box<dyn Operation>, (Span, Error)>;

fn make_token(context: &Context) -> Value {
    context.create_value(TokenType::new(context), None)
}

/// Region verifiers below only add the C-specific shape checks; that every
/// block is non-empty and terminated is already enforced by `Region::verify`.
fn verify_cond(context: &Context, op: &impl Operation, index: usize) -> Result<(), Error> {
    let block = op
        .regions()
        .nth(index)
        .unwrap()
        .iter(context.clone())
        .next()
        .unwrap();
    let last = context.get_op(*block.op_ids().last().unwrap());
    if last.dialect() != "cir" || last.name() != ConditionOp::name() {
        return Err(Error::VerificationError(
            "cir loop condition region must end with cir.condition".to_string(),
        ));
    }
    Ok(())
}

fn verify_body(context: &Context, body: &tir::Block) -> Result<(), Error> {
    let args = body.arguments();
    if args.len() != 1 || args[0].ty() != TokenType::new(context) {
        return Err(Error::VerificationError(
            "cir loop body must take a single !token argument".to_string(),
        ));
    }
    Ok(())
}

fn print_header(fmt: &mut IRFormatter, name: &str, token: ValueId) -> Result<(), std::fmt::Error> {
    fmt.write(format!("{name} %{}", token.number()))
}

fn print_region(
    fmt: &mut IRFormatter,
    context: &Context,
    op: &impl Operation,
    label: &str,
    index: usize,
) -> Result<(), std::fmt::Error> {
    fmt.write(format!(" {label}"))?;
    tir::region_format::print_op_region(fmt, context, op, index)
}

/// Parse a loop's `%handle` and materialise the fresh `!token` it names.
fn parse_handle(parser: &mut Parser, context: &Context) -> Result<Value, (Span, Error)> {
    parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    Ok(make_token(context))
}

fn parse_region(
    parser: &mut Parser,
    context: &Context,
    label: &'static str,
) -> Result<RegionId, (Span, Error)> {
    expect_token(parser, label)?;
    Ok(parser.parse_region(context)?.id())
}

fn parse_body(
    parser: &mut Parser,
    context: &Context,
    token: Value,
) -> Result<RegionId, (Span, Error)> {
    expect_token(parser, "body")?;
    Ok(parser
        .parse_region_with_entry_args(context, vec![token])?
        .id())
}

fn expect_token(parser: &mut Parser, token: &'static str) -> Result<(), (Span, Error)> {
    if parser.parse_token(token) {
        Ok(())
    } else {
        Err((parser.span(), Error::ExpectedToken(token)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tir::IRBuilder;
    use tir::builtin::{IntegerType, ops as b};
    use tir::parse::ir::parse_ir;

    fn context() -> Context {
        let context = Context::with_default_dialects();
        register(&context);
        context
    }

    /// A `cond` region computing a constant `i1` test, ended by `cir.condition`.
    fn cond_region(context: &Context) -> RegionId {
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        let mut builder = IRBuilder::new(block);
        let c = builder
            .insert(b::constant(context, 1, IntegerType::new(context, 1)).build())
            .result();
        builder.insert(ops::condition(context, c).build());
        region.id()
    }

    /// A `body`/`step` region holding the given terminator over its token arg.
    fn token_region<T: Operation + 'static>(
        context: &Context,
        terminator: impl FnOnce(&Context, ValueId) -> T,
    ) -> RegionId {
        let token = make_token(context);
        let token_id = token.id();
        let region = context.create_region();
        let block = context.create_block(vec![token]);
        region.add_block(block.id());
        IRBuilder::new(block).insert(terminator(context, token_id));
        region.id()
    }

    fn yield_region(context: &Context) -> RegionId {
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        IRBuilder::new(block).insert(ops::r#yield(context).build());
        region.id()
    }

    fn roundtrip<T: Operation + 'static>(context: &Context, op: &T, needles: &[&str]) {
        assert!(op.verify(context).is_ok(), "verify failed");
        let mut buf = String::new();
        op.print(&mut IRFormatter::new(&mut buf)).expect("print ok");
        for needle in needles {
            assert!(buf.contains(needle), "{needle:?} missing from:\n{buf}");
        }
        let parsed = parse_ir::<T>(context, &buf).expect("parse cir loop");
        assert!(parsed.verify(context).is_ok(), "reparsed verify failed");
    }

    #[test]
    fn while_break_roundtrip() {
        let context = context();
        let op = ops::r#while(
            &context,
            Some(cond_region(&context)),
            Some(token_region(&context, |c, t| ops::r#break(c, t).build())),
        )
        .build();
        roundtrip(
            &context,
            &op,
            &["cir.while", "cond", "body", "cir.condition", "cir.break"],
        );
    }

    #[test]
    fn do_continue_roundtrip() {
        let context = context();
        let op = ops::r#do(
            &context,
            Some(token_region(&context, |c, t| ops::r#continue(c, t).build())),
            Some(cond_region(&context)),
        )
        .build();
        roundtrip(&context, &op, &["cir.do", "body", "cond", "cir.continue"]);
    }

    #[test]
    fn for_roundtrip() {
        let context = context();
        let op = ops::r#for(
            &context,
            Some(cond_region(&context)),
            Some(token_region(&context, |c, _| ops::r#yield(c).build())),
            Some(yield_region(&context)),
        )
        .build();
        roundtrip(
            &context,
            &op,
            &["cir.for", "cond", "body", "step", "cir.yield"],
        );
    }

    #[test]
    fn break_requires_token_operand() {
        let context = context();
        let not_a_token = context.create_value(IntegerType::new(&context, 32), None);
        let _block = context.create_block(vec![not_a_token.clone()]);
        let op = ops::r#break(&context, not_a_token.id()).build();
        let error = op
            .verify(&context)
            .expect_err("break operand must be a token");
        assert!(
            error
                .to_string()
                .contains("expected constraint tir::builtin::TokenType")
        );
    }

    #[test]
    fn cond_region_must_end_with_condition() {
        let context = context();
        // A `cond` region that yields instead of producing a condition is invalid.
        let op = ops::r#while(
            &context,
            Some(yield_region(&context)),
            Some(token_region(&context, |c, t| ops::r#break(c, t).build())),
        )
        .build();
        let error = op
            .verify(&context)
            .expect_err("cond must end with cir.condition");
        assert!(error.to_string().contains("must end with cir.condition"));
    }

    #[test]
    fn body_needs_token_argument() {
        let context = context();
        let op = ops::r#while(
            &context,
            Some(cond_region(&context)),
            Some(yield_region(&context)),
        )
        .build();
        let error = op
            .verify(&context)
            .expect_err("body needs a token argument");
        assert!(error.to_string().contains("single !token argument"));
    }
}
