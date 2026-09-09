//! The generic views spec-05 gives functions, calls, globals and leaf ops:
//! what a consumer reads without knowing the concrete op.

use tir::{Apply, Callable, Global, MemoryState, Operation, Speculatable};

use super::fixtures::{self, module_ops};

const MODULE: &str = r#"module {
  %counter = global @counter align 4 bytes [1, 0, 0, 0]
  %fn_puts = func.declare @puts(!i32) -> !i32
  %fn_main = func.func @main(%0: !i32) -> !i32 {
    %1 = func.call %fn_puts(%0 : !i32) -> !i32
    %2 = addi %1, %0 : !i32
    %3 = divsi %2, %0 : !i32
    func.return %3
  }
  module_end
}"#;

#[test]
fn a_function_and_a_declaration_are_callable() {
    let (context, module) = fixtures::parse(MODULE);
    let ops = module_ops(&context, module.id());
    let i32_ty = tir::builtin::IntegerType::new(&context, 32);

    let declare = context
        .get_op(ops[1])
        .as_interface::<dyn Callable>()
        .expect("func.declare is callable");
    assert!(declare.body().is_none());
    assert_eq!(declare.params(), vec![i32_ty]);
    assert_eq!(declare.result(), i32_ty);
    assert_eq!(declare.value(), context.get_op(ops[1]).results()[0]);

    let func = context
        .get_op(ops[2])
        .as_interface::<dyn Callable>()
        .expect("func.func is callable");
    assert_eq!(func.body(), Some(context.get_op(ops[2]).regions()[0]));
    assert_eq!(func.params(), vec![i32_ty]);
}

#[test]
fn a_call_applies_its_callee_to_a_range_of_operands() {
    let (context, module) = fixtures::parse(MODULE);
    let ops = module_ops(&context, module.id());
    let body = context.get_op(ops[2]).regions()[0];
    let call = context.get_region(body).op_ids()[0];

    let apply = context
        .get_op(call)
        .as_interface::<dyn Apply>()
        .expect("func.call applies");
    assert_eq!(apply.callee(), context.get_op(ops[1]).results()[0]);
    assert_eq!(apply.args(), 1..2);
}

#[test]
fn a_global_publishes_its_address_and_initializer() {
    let (context, module) = fixtures::parse(MODULE);
    let ops = module_ops(&context, module.id());

    let global = context
        .get_op(ops[0])
        .as_interface::<dyn Global>()
        .expect("global is a data object");
    assert_eq!(global.address(), context.get_op(ops[0]).results()[0]);
    assert_eq!(global.initializer(), Some(vec![1, 0, 0, 0]));
}

#[test]
fn arithmetic_is_speculatable_and_division_is_not() {
    let (context, module) = fixtures::parse(MODULE);
    let ops = module_ops(&context, module.id());
    let body = context
        .get_region(context.get_op(ops[2]).regions()[0])
        .op_ids();

    assert!(context.get_op(body[1]).has_interface::<dyn Speculatable>());
    assert!(!context.get_op(body[2]).has_interface::<dyn Speculatable>());
    assert!(!context.get_op(body[0]).has_interface::<dyn Speculatable>());
}

#[test]
fn a_zero_filled_global_has_a_zero_image() {
    let (context, module) =
        fixtures::parse("module {\n  %s = global private @s size 3 align 1\n  module_end\n}");
    let ops = module_ops(&context, module.id());

    let global = context
        .get_op(ops[0])
        .as_interface::<dyn Global>()
        .expect("global is a data object");
    assert_eq!(global.initializer(), Some(vec![0, 0, 0]));
}

const THREADED_CALL: &str = r#"module {
  %fn_puts = func.declare @puts(!i32) -> !i32
  %fn_main = func.func @main(%0: !i32) -> !i32 {
    %1 = state.entry_state
    %2, %3 = func.call %fn_puts(%0 : !i32) -> !i32 state(%1)
    func.return %2 state(%3)
  }
  module_end
}"#;

#[test]
fn a_call_on_a_chain_reports_the_states_its_type_filter_finds() {
    let (context, module) = fixtures::parse(THREADED_CALL);
    let ops = module_ops(&context, module.id());
    let body = context.get_op(ops[1]).regions()[0];
    let call = context
        .get_region(body)
        .op_ids()
        .into_iter()
        .map(|op| context.get_op(op))
        .find(|op| op.is::<tir::func::CallOp>())
        .expect("the body holds the call");

    let memory = call
        .clone()
        .as_interface::<dyn MemoryState>()
        .expect("a call is ordered on memory");
    assert_eq!(memory.observed(), call.state_operands().to_vec());
    assert_eq!(memory.produced(), call.state_results().to_vec());
    assert_eq!(memory.observed().len(), 1);
    assert_eq!(memory.produced().len(), 1);
    assert!(memory.changes_memory());
    assert_eq!(call.value_operands().len(), 2);
    assert_eq!(call.value_results().len(), 1);
}
