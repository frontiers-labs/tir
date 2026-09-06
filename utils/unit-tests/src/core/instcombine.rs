//! InstCombine as a round of the mid-end pipeline: what it extracts from its
//! own output must leave that output alone.

use tir::{builtin::ModuleOp, func::FuncOp, parse::ir::parse_ir, Context, Operation, PassManager};

/// A counted loop with an early exit, as the frontend leaves it: the loop
/// carries a result it never changes on the way round, and the exit block
/// merges the early result with the carried one.
const EARLY_EXIT_LOOP: &str = r#"
module {
  %63 = func.func @f(%0: !i32) -> !i32 {
    %1 = constant {value = 0} : !i32
    %2 = constant {value = 1} : !i32
    %3 = constant {value = 6} : !i32
    cfg.br ^bb1(%1, %1 : !i32, !i32)
  ^bb1(%4: !i32, %5: !i32):
    %6 = cmpi %5, %0 {predicate = "slt"} : !i1
    cfg.cond_br %6, ^bb2, ^bb4(%4 : !i32)
  ^bb2:
    %7 = cmpi %5, %3 {predicate = "eq"} : !i1
    cfg.cond_br %7, ^bb4(%2 : !i32), ^bb3
  ^bb3:
    %8 = addi %5, %2 : !i32
    cfg.br ^bb1(%4, %8 : !i32, !i32)
  ^bb4(%9: !i32):
    func.return %9
  }
  module_end
}
"#;

#[test]
fn a_round_of_instcombine_reaches_a_fixpoint() {
    let context = Context::with_default_dialects();
    let module = parse_ir::<ModuleOp>(&context, EARLY_EXIT_LOOP).expect("the fixture parses");

    let mut raise = PassManager::new();
    raise
        .nest::<FuncOp>()
        .add_pass(tir::passes::RestructureNodesPass::new());
    raise
        .run(&context, context.get_op(module.id()))
        .expect("the fixture restructures");

    let mut round = PassManager::new();
    round
        .fixpoint(8)
        .nest::<FuncOp>()
        .add_pass(tir::passes::InstCombineNodesPass::new());
    round
        .run(&context, context.get_op(module.id()))
        .expect("the round simplifies");
    let settled = context.op_version(module.id());

    let mut once = PassManager::new();
    once.nest::<FuncOp>()
        .add_pass(tir::passes::InstCombineNodesPass::new());
    once.run(&context, context.get_op(module.id()))
        .expect("a settled function has nothing left to simplify");

    assert_eq!(
        context.op_version(module.id()),
        settled,
        "a fixpoint that ran to its cap left the function still moving"
    );
}
