//! The `cir` (C IR) dialect: C-specific constructs that have no direct
//! counterpart in the generic `builtin`/`scf` dialects.
//!
//! The first constructs are loop control flow with `break`. A [`LoopOp`] models
//! a C loop as an unconditional repeat over a single-block body; the body's
//! entry block carries one `!token` argument that is the *loop handle*. A
//! [`BreakOp`] exits, and a [`ContinueOp`] takes the back-edge, each naming the
//! loop they target by consuming its token. Because the token is an ordinary
//! SSA value, an inner loop can name an enclosing loop's handle, which is how a
//! `break`/`continue` aimed at an outer loop is expressed.
//!
//! The token is the link the builtin token type was added for: given a `break`,
//! its token operand's definition (the loop body's entry argument) is the loop
//! it leaves, so passes recover the control-flow target without a side table.

use tir::builtin::TokenType;
use tir::parse::common::Cursor;
use tir::{Context, Error, IRFormatter, Operation, Terminator, ValueId, dialect, operation};

pub mod ops {
    pub use super::{BreakOp, ContinueOp, LoopOp, r#break, r#continue, r#loop};
}

dialect! {
    CirDialect {
        name: "cir",
        operations: [
            LoopOp,
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
    LoopOp {
        name: "loop",
        dialect: "cir",
        format: "custom",
        verifier: "true",
        regions: R {
            body: Region {
                single_block: true,
            }
        }
    }
}

impl tir::Verifiable for LoopOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let body = self.body();
        let args = body.arguments();
        if args.len() != 1 || args[0].ty() != TokenType::new(context) {
            return Err(Error::VerificationError(
                "cir.loop body must take a single !token argument".to_string(),
            ));
        }
        if body
            .op_ids()
            .last()
            .map(|id| {
                context
                    .get_op(*id)
                    .as_interface::<dyn Terminator>()
                    .is_none()
            })
            .unwrap_or(true)
        {
            return Err(Error::VerificationError(
                "cir.loop body must end with a terminator".to_string(),
            ));
        }
        Ok(())
    }
}

impl LoopOp {
    /// The loop handle: the `!token` argument of the body's entry block.
    pub fn token(&self) -> ValueId {
        self.body().arguments()[0].id()
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.write(format!("cir.loop %{}", self.token().number()))?;
        tir::region_format::print_op_region(fmt, &self.0.context.upgrade(), self, 0)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
        let token = context.create_value(TokenType::new(context), None);
        let body = parser.parse_region_with_entry_args(context, vec![token])?;
        Ok(Box::new(
            LoopOpBuilder::new(context).body(body.id()).build(),
        ))
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use tir::parse::ir::parse_ir;
    use tir::{IRBuilder, Operation};

    fn context() -> Context {
        let context = Context::with_default_dialects();
        register(&context);
        context
    }

    /// Build a `cir.loop` whose body ends with the given terminator-producing
    /// closure, fed the loop's token.
    fn loop_with<T: Operation + 'static>(
        context: &Context,
        terminator: impl FnOnce(&Context, ValueId) -> T,
    ) -> LoopOp {
        let token = context.create_value(TokenType::new(context), None);
        let token_id = token.id();
        let region = context.create_region();
        let block = context.create_block(vec![token]);
        region.add_block(block.id());
        IRBuilder::new(block).insert(terminator(context, token_id));
        ops::r#loop(context, Some(region.id())).build()
    }

    #[test]
    fn loop_break_roundtrip() {
        let context = context();
        let op = loop_with(&context, |c, t| ops::r#break(c, t).build());

        assert!(op.verify(&context).is_ok());

        let mut buf = String::new();
        op.print(&mut IRFormatter::new(&mut buf)).expect("print ok");
        assert!(buf.contains("cir.loop"));
        assert!(buf.contains("cir.break"));

        let parsed = parse_ir::<LoopOp>(&context, &buf).expect("parse cir.loop");
        assert!(parsed.verify(&context).is_ok());
    }

    #[test]
    fn loop_continue_roundtrip() {
        let context = context();
        let op = loop_with(&context, |c, t| ops::r#continue(c, t).build());

        assert!(op.verify(&context).is_ok());

        let mut buf = String::new();
        op.print(&mut IRFormatter::new(&mut buf)).expect("print ok");
        assert!(buf.contains("cir.continue"));

        let parsed = parse_ir::<LoopOp>(&context, &buf).expect("parse cir.loop");
        assert!(parsed.verify(&context).is_ok());
    }

    #[test]
    fn break_requires_token_operand() {
        let context = context();
        let not_a_token = context.create_value(tir::builtin::IntegerType::new(&context, 32), None);
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
    fn loop_body_needs_token_argument() {
        let context = context();
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        let token = context.create_value(TokenType::new(&context), None);
        IRBuilder::new(block).insert(ops::r#break(&context, token.id()).build());
        let op = ops::r#loop(&context, Some(region.id())).build();

        let error = op
            .verify(&context)
            .expect_err("body needs a token argument");
        assert!(error.to_string().contains("single !token argument"));
    }
}
