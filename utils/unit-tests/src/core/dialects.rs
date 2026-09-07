//! Dialect behavior: cfg, scf, ptr, func.declare and custom op interfaces.

use tir::{
    builtin::{IntegerType, UnitType},
    cfg::ops as cfg_ops,
    func::ops as func_ops,
    ptr::{AllocaOpBuilder, LoadOpBuilder, PtrType, StoreOpBuilder},
    Context, MemoryRead, MemoryWrite, Operation,
};

#[test]
fn branch_terminates_function_block() {
    let context = Context::with_default_dialects();
    let region = context.create_region();
    let entry = context.create_block(vec![]);
    region.add_block(entry.id());
    let target = context.create_block(vec![]);

    let func = func_ops::lambda(&context, "jump", UnitType::new(&context), &region).build();

    func.body()
        .append_op(cfg_ops::br(&context, vec![], target.id()).build());

    assert!(func.verify(&context).is_ok());
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
        .dep_result()
        .build();
    let entry_state = allocation.state_result().unwrap();

    let store = StoreOpBuilder::new(&context)
        .value(value_id)
        .ptr(allocation.result())
        .dep_operand(entry_state)
        .dep_result()
        .build();
    let load = LoadOpBuilder::new(&context)
        .ptr(allocation.result())
        .result_type(i32_ty)
        .dep_operand(store.state_result().unwrap())
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
