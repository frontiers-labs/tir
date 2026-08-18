//! CIR dialect verifier behavior. The dialect is registered by fcc only, so
//! these modules are invisible to the generic `tir` tool and cannot be LIT
//! checks.

use tir::{builtin::ModuleOp, parse::ir::parse_ir, verify_op_tree, Context, Operation};

fn verify(module: &str) -> Result<(), tir::Error> {
    let context = Context::with_default_dialects();
    context.register_dialect::<fcc::cir::CirDialect>();
    let module = parse_ir::<ModuleOp>(&context, module).expect("parse module");
    verify_op_tree(&context, module.id())
}

#[test]
fn variadic_call_accepts_arguments_beyond_the_fixed_prefix() {
    verify(
        r#"module {
  func.declare @printf(!ptr.p, !cir.varargs) -> !i32
  func.func @caller(%0: !ptr.p, %1: !i32) -> !i32 {
    %2 = func.call @printf(%0, %1 : !ptr.p, !i32) -> !i32
    func.return %2
  }
  module_end
}"#,
    )
    .expect("a variadic call verifies");
}

#[test]
fn variadic_call_accepts_an_empty_tail() {
    verify(
        r#"module {
  func.declare @printf(!ptr.p, !cir.varargs) -> !i32
  func.func @caller(%0: !ptr.p) -> !i32 {
    %1 = func.call @printf(%0 : !ptr.p) -> !i32
    func.return %1
  }
  module_end
}"#,
    )
    .expect("a variadic call with no variadic argument verifies");
}

#[test]
fn variadic_call_rejects_a_mismatched_fixed_prefix() {
    verify(
        r#"module {
  func.declare @printf(!ptr.p, !cir.varargs) -> !i32
  func.func @caller(%0: !i64, %1: !i32) -> !i32 {
    %2 = func.call @printf(%0, %1 : !i64, !i32) -> !i32
    func.return %2
  }
  module_end
}"#,
    )
    .expect_err("the fixed prefix must still match");
}
