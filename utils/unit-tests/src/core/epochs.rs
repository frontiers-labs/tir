//! The edit contract: a pass reads its own edits at once, and a commit moves
//! no id and changes nothing a read answers.

use tir::{
    builtin::{self, IntegerType},
    func, scf, Analysis, AnalysisManager, BlockHandle, Context, OpId, Operand, Operation, Use,
    ValueId,
};

use super::fixtures;

fn committed_function(context: &Context) -> (OpId, BlockHandle) {
    let module = fixtures::parse_in(
        context,
        "module {\n%fn_demo = func.func @demo() -> !i32 {\n}\nmodule_end\n}",
    );
    let func = fixtures::module_ops(context, module.id())[0];
    let body = context
        .get_op(func)
        .as_op::<func::FuncOp>()
        .expect("a func")
        .body();
    (func, body)
}

fn add_fixture(context: &Context) -> (ValueId, ValueId, OpId) {
    let (_, body) = committed_function(context);
    let i32_ty = IntegerType::new(context, 32);
    let c = builtin::ops::constant(context, 1, i32_ty).build();
    body.append(c.id());
    let d = builtin::ops::constant(context, 2, i32_ty).build();
    body.append(d.id());
    let add = builtin::ops::addi(context, c.result(), c.result(), i32_ty).build();
    body.append(add.id());
    context.commit();
    let ops = body.op_ids();
    let (c, d, add) = (ops[0], ops[1], ops[2]);
    let result = |op| context.get_op(op).results()[0];
    (result(c), result(d), add)
}

#[test]
fn commit_keeps_every_id_and_every_handle() {
    let context = Context::with_default_dialects();
    let (func, body) = committed_function(&context);
    let i32_ty = IntegerType::new(&context, 32);
    let c = builtin::ops::constant(&context, 1, i32_ty).build();
    body.append(c.id());
    let add = builtin::ops::addi(&context, c.result(), c.result(), i32_ty).build();
    body.append(add.id());
    let handle = context.get_op(c.id());

    context.commit();

    assert!(context.has_operation(func));
    assert_eq!(body.op_ids(), [c.id(), add.id()]);
    assert_eq!(
        context.get_op(add.id()).operands().as_slice(),
        [c.result(); 2]
    );
    assert_eq!(
        context.uses_of(c.result()),
        [Use::new(add.id(), 0), Use::new(add.id(), 1)]
    );
    assert_eq!(context.get_value(c.result()).defining_op(), Some(c.id()));
    assert!(handle.is_live(), "a committed entity keeps its handle");
    assert!(body.is_live());
}

#[test]
fn changing_an_operand_moves_the_use_before_commit() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);

    context.set_op_operand(add, 1, d);

    assert_eq!(context.get_op(add).operands().as_slice(), [c, d]);
    assert_eq!(context.uses_of(c), [Use::new(add, 0)]);
    assert_eq!(context.uses_of(d), [Use::new(add, 1)]);

    context
        .erase_op(&tir::OperationRef::new(context.get_op(add)))
        .unwrap();
    assert!(!context.is_used(c));
    assert!(!context.is_used(d));
    assert!(!context.has_operation(add));
}

#[test]
fn setting_a_duplicate_slot_moves_one_use() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);

    context.set_op_operand(add, 0, d);
    context.set_op_operand(add, 0, c);
    context.set_op_operand(add, 1, d);

    assert_eq!(context.get_op(add).operands().as_slice(), [c, d]);
    assert_eq!(context.uses_of(c), [Use::new(add, 0)]);
    assert_eq!(context.uses_of(d), [Use::new(add, 1)]);
    context.commit();
    assert_eq!(context.uses_of(c), [Use::new(add, 0)]);
    assert_eq!(context.uses_of(d), [Use::new(add, 1)]);
}

#[test]
fn moving_an_op_between_blocks_updates_both_before_commit() {
    let context = Context::with_default_dialects();
    let (c, _, add) = add_fixture(&context);
    let source = context.get_block(context.parent_block(add).unwrap());
    let target = context.create_block(vec![]);

    assert!(source.remove_op(add));
    target.append(add);

    assert!(!source.op_ids().contains(&add));
    assert_eq!(target.op_ids(), [add]);
    assert_eq!(context.parent_block(add), Some(target.id()));
    assert_eq!(context.uses_of(c), [Use::new(add, 0), Use::new(add, 1)]);
    context.commit();
    assert_eq!(context.parent_block(add), Some(target.id()));
    assert!(!source.op_ids().contains(&add));
}

#[test]
fn a_duplicate_slot_stays_a_distinct_use_through_rauw() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);

    context.replace_value_uses(c, d);

    assert_eq!(context.get_op(add).operands().as_slice(), [d, d]);
    assert!(!context.is_used(c));
    assert_eq!(context.uses_of(d), [Use::new(add, 0), Use::new(add, 1)]);
    assert_eq!(context.use_count(d), 2);
}

#[test]
fn rauw_resolves_transitively_and_commit_agrees() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);
    let i32_ty = IntegerType::new(&context, 32);
    let e = builtin::ops::constant(&context, 3, i32_ty).build();
    context
        .get_block(context.parent_block(add).unwrap())
        .append(e.id());

    context.replace_value_uses(c, d);
    context.replace_value_uses(d, e.result());

    assert_eq!(context.get_op(add).operands().as_slice(), [e.result(); 2]);
    assert!(!context.is_used(c) && !context.is_used(d));
    assert_eq!(context.use_count(e.result()), 2);

    context.commit();
    assert_eq!(context.get_op(add).operands().as_slice(), [e.result(); 2]);
    assert_eq!(
        context.uses_of(e.result()),
        [Use::new(add, 0), Use::new(add, 1)]
    );
    assert!(!context.is_used(c) && !context.is_used(d));
}

#[test]
fn a_use_recorded_after_rauw_names_the_old_value() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);
    let i32_ty = IntegerType::new(&context, 32);
    context.replace_value_uses(c, d);

    let late = builtin::ops::addi(&context, c, c, i32_ty).build();
    context
        .get_block(context.parent_block(add).unwrap())
        .append(late.id());

    assert_eq!(context.get_op(late.id()).operands().as_slice(), [c; 2]);
    assert_eq!(
        context.uses_of(c),
        [Use::new(late.id(), 0), Use::new(late.id(), 1)]
    );
    assert_eq!(context.uses_of(d), [Use::new(add, 0), Use::new(add, 1)]);

    let e = builtin::ops::constant(&context, 3, i32_ty).build();
    context.replace_value_uses(c, e.result());
    assert_eq!(context.get_op(add).operands().as_slice(), [d; 2]);
    assert_eq!(
        context.get_op(late.id()).operands().as_slice(),
        [e.result(); 2]
    );

    context.commit();
    assert_eq!(context.get_op(add).operands().as_slice(), [d; 2]);
    assert_eq!(
        context.get_op(late.id()).operands().as_slice(),
        [e.result(); 2]
    );
}

#[test]
fn replacing_a_pending_replacement_redirects_the_base_slots() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);
    let i32_ty = IntegerType::new(&context, 32);
    let e = builtin::ops::constant(&context, 3, i32_ty).build();
    let (e_id, e_result) = (e.id(), e.result());
    context
        .get_block(context.parent_block(add).unwrap())
        .append(e_id);

    context.replace_value_uses(c, e_result);
    context.replace_value_uses(e_result, d);

    assert_eq!(context.get_op(add).operands().as_slice(), [d; 2]);
    assert_eq!(context.uses_of(d), [Use::new(add, 0), Use::new(add, 1)]);
    assert!(!context.is_used(e_result));
    context
        .erase_op(&tir::OperationRef::new(context.get_op(e_id)))
        .unwrap();
    assert!(context.has_value(d) && !context.has_value(e_result));
    assert_eq!(context.get_op(add).operands().as_slice(), [d; 2]);

    context.commit();
    assert_eq!(context.get_op(add).operands().as_slice(), [d; 2]);
}

#[test]
fn a_shadowed_user_counts_once() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);
    let i32_ty = IntegerType::new(&context, 32);

    let e = builtin::ops::constant(&context, 3, i32_ty).build();
    context.append_operand(add, e.result());

    assert_eq!(
        context.get_op(add).operands().as_slice(),
        [c, c, e.result()]
    );
    assert_eq!(context.uses_of(c), [Use::new(add, 0), Use::new(add, 1)]);
    assert_eq!(context.uses_of(e.result()), [Use::new(add, 2)]);
    assert!(!context.is_used(d));
}

#[test]
fn erasing_a_base_user_hides_its_uses_before_commit() {
    let context = Context::with_default_dialects();
    let (c, _, add) = add_fixture(&context);
    let block = context.parent_block(add).unwrap();

    context
        .erase_op(&tir::OperationRef::new(context.get_op(add)))
        .unwrap();

    assert!(!context.is_used(c));
    assert!(!context.has_operation(add));
    assert!(!context.get_block(block).op_ids().contains(&add));
    context.commit();
    assert!(!context.has_operation(add));
    assert!(!context.is_used(c));
}

const TWO_REGIONS: &str = r#"module {
  %fn_main = func.func @main(%0: !i32) -> !i32 {
    %1 = constant {value = 0} : !i1
    %2 = scf.loop (%3 = %0) {
      %4 = constant {value = 7} : !i32
      -> %1, %3, %3
    }
    -> %2
  }
  module_end
}"#;

#[test]
fn moving_an_op_updates_both_parents_immediately() {
    let (context, module) = fixtures::parse(TWO_REGIONS);
    let function = fixtures::module_ops(&context, module.id())[0];
    let outer = context.get_op(function).regions()[0];
    let loop_op = context
        .get_region(outer)
        .op_ids()
        .into_iter()
        .find(|&op| context.get_op(op).is::<scf::LoopOp>())
        .unwrap();
    let body = context.get_op(loop_op).regions()[0];
    let inner_constant = context.get_region(body).op_ids()[0];

    context.remove_from_region(body, inner_constant);
    context.add(outer, inner_constant);

    assert!(!context.get_region(body).op_ids().contains(&inner_constant));
    assert!(context.get_region(outer).op_ids().contains(&inner_constant));
    assert_eq!(context.parent_nodes_region(inner_constant), Some(outer));
    assert_eq!(context.parent_op(inner_constant), Some(function));

    context.commit();
    assert_eq!(context.parent_nodes_region(inner_constant), Some(outer));
    assert!(!context.get_region(body).op_ids().contains(&inner_constant));
}

fn text(context: &Context, function: OpId) -> String {
    let func = context.get_op(function).as_op::<func::FuncOp>().unwrap();
    let mut out = String::new();
    let mut fmt = tir::IRFormatter::new(&mut out);
    tir::print_ir(&func, context, &mut fmt).unwrap();
    out
}

#[test]
fn a_grown_port_is_aligned_before_and_after_commit() {
    let (context, module) = fixtures::parse(TWO_REGIONS);
    let function = fixtures::module_ops(&context, module.id())[0];
    let outer = context.get_op(function).regions()[0];
    let loop_op = context
        .get_region(outer)
        .op_ids()
        .into_iter()
        .find(|&op| context.get_op(op).is::<scf::LoopOp>())
        .unwrap();
    let init = context.get_region(outer).ports()[0].id();
    let i32_ty = IntegerType::new(&context, 32);

    let result = context.grow_port(loop_op, i32_ty, Some(init), |_, port| port);
    let before = text(&context, function);
    tir::verify_op_tree(&context, function).expect("aligned before commit");

    context.commit();
    let after = text(&context, function);
    tir::verify_op_tree(&context, function).expect("aligned after commit");
    assert_eq!(before, after, "the visible graph is the committed graph");
    assert!(context.get_op(loop_op).results().contains(&result));
}

struct Count(usize);

impl Analysis for Count {
    fn build(_: &AnalysisManager, context: &Context, op: OpId) -> Self {
        Count(
            context
                .get_region(context.get_op(op).regions()[0])
                .iter(context.clone())
                .next()
                .map_or(0, |block| block.op_ids().len()),
        )
    }
}

#[test]
fn an_analysis_sees_an_edit_before_commit_and_survives_it() {
    let context = Context::with_default_dialects();
    let (func, body) = committed_function(&context);
    let analyses = AnalysisManager::new();
    assert_eq!(analyses.get::<Count>(&context, func).0, 0);

    body.append(func::ops::r#return(&context, Operand::none()).build().id());
    assert_eq!(analyses.get::<Count>(&context, func).0, 1);

    let cached = analyses.get::<Count>(&context, func);
    context.commit();
    let after = analyses
        .get_cached::<Count>(&context, func)
        .expect("the graph the result describes did not change");
    assert!(std::rc::Rc::ptr_eq(&cached, &after));
}

#[test]
fn a_no_op_edit_keeps_the_revision() {
    let context = Context::with_default_dialects();
    let (c, _, add) = add_fixture(&context);
    let before = context.op_version(add);

    context.set_op_operand(add, 0, c);

    assert_eq!(context.op_version(add), before);
}
