//! Dialect behavior: cfg, scf, ptr, func.declare and custom op interfaces.

use tir::{
    attributes::AttributeValue,
    builtin::{ops as builtin_ops, IndexType, IntegerType, UnitType},
    cfg::ops as cfg_ops,
    func::ops as func_ops,
    ptr::{AllocaOpBuilder, LoadOpBuilder, PtrType, StoreOpBuilder},
    scf::{ops as scf_ops, ForOpBuilder, IfOpBuilder, WhileOpBuilder},
    Context, EntryGuard, GuardOrdering, GuardedLoop, MemoryRead, MemoryWrite, Operation,
};

fn terminated_region(context: &Context) -> tir::RegionId {
    let region = context.create_region();
    let block = context.create_block(vec![]);
    region.add_block(block.id());
    block.append_op(scf_ops::r#yield(context, vec![]).build());
    region.id()
}

#[test]
fn branch_terminates_function_block() {
    let context = Context::with_default_dialects();
    let region = context.create_region();
    let entry = context.create_block(vec![]);
    region.add_block(entry.id());
    let target = context.create_block(vec![]);

    let func = func_ops::func(&context, "jump", UnitType::new(&context), Some(region.id())).build();

    func.body()
        .append_op(cfg_ops::br(&context, vec![], target.id()).build());

    assert!(func.verify(&context).is_ok());
}

#[test]
fn scf_ops_nest_in_function() {
    let context = Context::with_default_dialects();
    let condition = context.create_value(IntegerType::new(&context, 1), None);
    let region = context.create_region();
    let block = context.create_block(vec![condition.clone()]);
    region.add_block(block.id());
    let func = func_ops::func(
        &context,
        "control",
        UnitType::new(&context),
        Some(region.id()),
    )
    .build();

    let if_op = scf_ops::r#if(
        &context,
        condition.id(),
        vec![],
        vec![],
        Some(terminated_region(&context)),
        Some(terminated_region(&context)),
    )
    .build();

    func.body().append_op(if_op);
    func.body()
        .append_op(func_ops::r#return(&context, tir::Operand::none()).build());

    assert!(func.verify(&context).is_ok());
}

#[test]
fn for_guard_reads_a_bound_comparison_over_its_operands() {
    let context = Context::with_default_dialects();
    let index = IndexType::new(&context);
    let lower = context.create_value(index, None);
    let upper = context.create_value(index, None);
    let step = context.create_value(index, None);
    let for_op = ForOpBuilder::new(&context)
        .lower_bound(lower.id())
        .upper_bound(upper.id())
        .step(step.id())
        .body(terminated_region(&context))
        .inits(vec![])
        .result_types(vec![])
        .build();

    let guard = context
        .get_op(for_op.id())
        .as_interface::<dyn GuardedLoop>()
        .expect("scf.for implements GuardedLoop")
        .entry_guard();

    assert_eq!(
        guard,
        EntryGuard::Less {
            ordering: GuardOrdering::Signed,
            lhs: lower.id(),
            rhs: upper.id(),
        }
    );
}

#[test]
fn while_guard_reads_the_condition_region_over_the_inits() {
    let context = Context::with_default_dialects();
    let i32_type = IntegerType::new(&context, 32);
    let init = context.create_value(i32_type, None);
    let condition_arg = context.create_value(i32_type, None);

    let condition_region = context.create_region();
    let condition_block = context.create_block(vec![condition_arg.clone()]);
    condition_region.add_block(condition_block.id());
    let decision = condition_block
        .append_op(builtin_ops::constant(&context, 1, IntegerType::new(&context, 1)).build())
        .result();
    condition_block
        .append_op(scf_ops::condition(&context, decision, vec![condition_arg.id()]).build());

    let body_arg = context.create_value(i32_type, None);
    let body_region = context.create_region();
    let body_block = context.create_block(vec![body_arg.clone()]);
    body_region.add_block(body_block.id());
    body_block.append_op(scf_ops::r#yield(&context, vec![body_arg.id()]).build());

    let while_op = WhileOpBuilder::new(&context)
        .condition_region(condition_region.id())
        .body(body_region.id())
        .inits(vec![init.id()])
        .result_types(vec![i32_type])
        .build();

    let guard = context
        .get_op(while_op.id())
        .as_interface::<dyn GuardedLoop>()
        .expect("scf.while implements GuardedLoop")
        .entry_guard();

    assert_eq!(
        guard,
        EntryGuard::Region {
            region: condition_region.id(),
            arguments: vec![condition_arg.id()],
            condition: decision,
        }
    );
}

/// A γ forwards each input to every arm as an entry argument; the text form
/// cannot even spell an arm with the wrong arity, so the invalid op is built
/// directly and must fail verification.
#[test]
fn an_arm_takes_one_argument_per_forwarded_input() {
    let context = Context::with_default_dialects();
    let condition = context.create_value(IntegerType::new(&context, 1), None);
    let input = context.create_value(IntegerType::new(&context, 32), None);
    let region = context.create_region();
    let block = context.create_block(vec![condition.clone(), input.clone()]);
    region.add_block(block.id());
    let func = func_ops::func(&context, "gate", UnitType::new(&context), Some(region.id())).build();

    // The arms take no arguments, so neither names the forwarded input.
    let if_op = IfOpBuilder::new(&context)
        .condition(condition.id())
        .inputs(vec![input.id()])
        .then_body(terminated_region(&context))
        .else_body(terminated_region(&context))
        .result_types(vec![])
        .build();

    func.body().append_op(if_op);
    func.body()
        .append_op(func_ops::r#return(&context, tir::Operand::none()).build());

    assert!(tir::verify_op_tree(&context, func.id()).is_err());
}

#[test]
fn opaque_and_typed_pointer_roundtrip() {
    let context = Context::with_default_dialects();

    let opaque = PtrType::opaque(&context);
    assert_eq!(context.type_to_string(opaque), "!ptr.p");

    let i32_ty = IntegerType::new(&context, 32);
    let typed = PtrType::typed(&context, i32_ty);
    assert_eq!(context.type_to_string(typed), "!ptr.p<!i32>");

    // Typed pointer remembers its pointee.
    let data = context.get_type_data(typed);
    let ptr = (data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<PtrType>()
        .unwrap();
    assert_eq!(ptr.pointee(&context), Some(i32_ty));

    // An opaque pointer carries no pointee.
    let opaque_data = context.get_type_data(opaque);
    let opaque_ptr = (opaque_data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<PtrType>()
        .unwrap();
    assert_eq!(opaque_ptr.pointee(&context), None);

    // Typed and opaque pointers are distinct, identical ones are interned.
    assert_ne!(opaque, typed);
    assert_eq!(PtrType::typed(&context, i32_ty), typed);
}

#[test]
fn deeply_nested_pointers_are_interned() {
    let context = Context::with_default_dialects();
    let build = |depth| {
        let mut ty = IntegerType::new(&context, 32);
        for _ in 0..depth {
            ty = PtrType::typed(&context, ty);
        }
        ty
    };

    assert_eq!(build(10_000), build(10_000));
}

#[test]
fn memory_interfaces_expose_the_state_chain() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let ptr_ty = PtrType::typed(&context, i32_ty);
    let value = context.create_value(i32_ty, None);
    let value_id = value.id();
    let _block = context.create_block(vec![value]);

    let allocation = AllocaOpBuilder::new(&context)
        .size(4)
        .align(4)
        .result_type(ptr_ty)
        .state_result()
        .build();
    let entry_state = allocation.state_result().unwrap();

    let store = StoreOpBuilder::new(&context)
        .value(value_id)
        .ptr(allocation.result())
        .state(entry_state)
        .state_result()
        .build();
    let load = LoadOpBuilder::new(&context)
        .ptr(allocation.result())
        .result_type(i32_ty)
        .state(store.state_result().unwrap())
        .build();

    let write: &dyn MemoryWrite = &store;
    assert_eq!(write.state_operand(), Some(entry_state));
    assert_eq!(write.state_result(), store.state_result());

    let read: &dyn MemoryRead = &load;
    assert_eq!(read.state_operand(), store.state_result());
}

#[test]
fn state_ports_are_absent_until_threaded() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let ptr_ty = PtrType::typed(&context, i32_ty);
    let allocation = AllocaOpBuilder::new(&context)
        .size(4)
        .align(4)
        .result_type(ptr_ty)
        .build();
    let load = LoadOpBuilder::new(&context)
        .ptr(allocation.result())
        .result_type(i32_ty)
        .build();

    assert_eq!(allocation.state_result(), None);
    assert_eq!(load.state_operand(), None);
}

/// A half-signature declare is neither a function nor a data declaration,
/// and the text form cannot express it: only a builder can build one.
#[test]
fn declare_carrying_a_return_type_without_arguments_is_rejected() {
    let context = Context::with_default_dialects();
    let declaration = tir::func::DeclareOpBuilder::new(&context)
        .attr(
            "sym_name",
            AttributeValue::Str("counter".to_string().into()),
        )
        .attr(
            "ret_type",
            AttributeValue::Type(IntegerType::new(&context, 32)),
        )
        .build();

    let error = declaration
        .verify(&context)
        .expect_err("a data declaration must not carry a return type");

    assert!(
        format!("{error:?}").contains("carries 'ret_type' without 'arg_types'"),
        "{error:?}"
    );
}

tir::helpers::operation! {
    DoWhileOp {
        name: "do_while",
        dialect: "test",
        regions: R {
            body: Region {
                single_block: true,
            }
        },
        interfaces: [tir::GuardedLoop],
    }
}

/// A tail-controlled loop: its body always runs once, so the guard reading says
/// there is no zero-trip check.
impl GuardedLoop for DoWhileOp {
    fn entry_guard(&self) -> EntryGuard {
        EntryGuard::AlwaysTaken
    }
}

#[test]
fn a_tail_controlled_loop_reads_as_unconditionally_entered() {
    let context = Context::with_default_dialects();
    DoWhileOp::register_interfaces(&context);
    let region = context.create_region();
    region.add_block(context.create_block(vec![]).id());
    let op = DoWhileOpBuilder::new(&context).body(region.id()).build();

    let guard = context
        .get_op(op.id())
        .as_interface::<dyn GuardedLoop>()
        .expect("the test loop implements GuardedLoop")
        .entry_guard();

    assert_eq!(guard, EntryGuard::AlwaysTaken);
}
