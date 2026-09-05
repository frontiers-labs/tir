//! Unordered regions built through the context rather than parsed: what a
//! producer of the form will do once one exists.

use tir::{
    builtin::{self, ops},
    func,
    interp::{self, Value},
    Context, OpId, Operation,
};

/// `module { %f = func.func @quad(%p: !i32) { %a = addi %p, %p; %b = addi %a, %a; -> %b } }`
fn quad(context: &Context) -> (OpId, OpId) {
    let i32_ty = builtin::IntegerType::new(context, 32);
    let port = context.create_value(i32_ty, None);
    let double = ops::addi(context, port.id(), port.id(), i32_ty).build();
    let quad = ops::addi(context, double.result(), double.result(), i32_ty).build();
    let body = context.create_nodes_region(
        vec![port],
        0,
        vec![double.id(), quad.id()],
        vec![quad.result()],
        0,
    );
    let function = func::ops::lambda(context, "quad", i32_ty, &body).build();
    let module = builtin::ops::module(context, None).build();
    module.body().append(function.id());
    module
        .body()
        .append_op(builtin::ModuleEndOpBuilder::new(context).build());
    (module.id(), function.id())
}

#[test]
fn an_unordered_function_body_verifies_and_runs() {
    let context = Context::with_default_dialects();
    let (module, function) = quad(&context);

    tir::verify_op_tree(&context, module).expect("an unordered body verifies");

    let results = interp::run_function(
        &context,
        function,
        vec![Value::Int(tir::utils::APInt::new(32, 5))],
    )
    .expect("an unordered body runs");
    assert_eq!(results[0].to_i64(), Some(20));
}

#[test]
fn copying_an_unordered_region_copies_its_ports_and_operations() {
    let context = Context::with_default_dialects();
    let (_, function) = quad(&context);
    let body = context.get_op(function).regions()[0];

    let copy = context.get_region(tir::clone_region_with_mapping(
        &context,
        body,
        &Default::default(),
    ));

    assert!(copy.is_nodes());
    assert_eq!(copy.op_ids().len(), 2);
    let original = context.get_region(body);
    assert_ne!(copy.ports()[0].id(), original.ports()[0].id());
    assert_ne!(copy.results()[0], original.results()[0]);
}

#[test]
fn erasing_the_owner_reclaims_an_unordered_region() {
    let context = Context::with_default_dialects();
    let (_, function) = quad(&context);
    let body = context.get_region(context.get_op(function).regions()[0]);
    let held = body.op_ids();
    let port = body.ports()[0].id();

    let mut rewriter = tir::Rewriter::new(context.clone());
    rewriter
        .erase_op(&tir::OperationRef::new(context.get_op(function)))
        .expect("the function leaves the module");

    for op in held {
        assert!(
            !context.has_operation(op),
            "the region's operations go with it"
        );
    }
    assert!(!context.has_value(port), "so do the region's ports");
    assert!(!context.is_region_port(port), "and their def-site index");
}
