//! `binds:` on a fixture op: the macro derives the binding, and the verifier
//! holds every aligned range to one length and one type per offset.

use tir::{
    builtin::{self, ops},
    Binding, Context, Gamma, Operation, RegionId, Theta, TypeId, ValueId,
};

tir::helpers::op_def! {
    file: "../../core/defs/converted-tests/src_core_binding.cdsl",
    hosts: {

    },
}

fn nodes_region(context: &Context, port_types: &[TypeId], results: &[(i64, u32)]) -> RegionId {
    let ports = port_types
        .iter()
        .map(|&ty| context.create_value(ty, None))
        .collect();
    let constants: Vec<_> = results
        .iter()
        .map(|&(value, width)| {
            ops::constant(
                context,
                value,
                builtin::IntegerType::new(context, u64::from(width)),
                Default::default(),
                tir::InsertionPoint::Root,
            )
        })
        .collect();
    let values: Vec<ValueId> = constants.iter().map(|c| c.value_results()[0]).collect();
    let op_ids = constants.iter().map(|c| c.id()).collect();
    context.create_nodes_region(ports, op_ids, values).id()
}

fn build_theta(context: &Context, port_types: &[TypeId], results: &[(i64, u32)]) -> tir::OpHandle {
    context.register_operation(&THETA);
    let i32_ty = builtin::IntegerType::new(context, 32);
    let init = ops::constant(
        context,
        7,
        i32_ty,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    r#theta(
        context,
        vec![init.value_results()[0]],
        vec![i32_ty],
        Some(nodes_region(context, port_types, results)),
        Default::default(),
        tir::InsertionPoint::Root,
    )
}

#[test]
fn a_theta_binding_aligns_inits_ports_continue_exit_and_results() {
    let context = Context::with_default_dialects();
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let op = build_theta(&context, &[i32_ty], &[(1, 1), (2, 32), (3, 32)]);

    let theta = op
        .clone()
        .as_interface::<dyn Theta>()
        .expect("theta interface");
    assert_eq!(
        theta.binding(),
        Binding {
            operands: 0..1,
            ports: 0..1,
            continue_: 1..2,
            exit: 2..3,
            results: 0..1,
        }
    );
    let body = context.get_region(theta.body());
    assert_eq!(theta.predicate(), body.value_results()[0]);
    op.verify(&context).expect("an aligned theta verifies");
}

#[test]
fn a_theta_port_must_have_its_init_type() {
    let context = Context::with_default_dialects();
    let i64_ty = builtin::IntegerType::new(&context, 64);
    let op = build_theta(&context, &[i64_ty], &[(1, 1), (2, 32), (3, 32)]);

    let error = op
        .verify(&context)
        .expect_err("the port is wider than its init");
    assert!(error.to_string().contains("port"), "{error}");
}

fn build_gamma(context: &Context, arm_results: &[&[(i64, u32)]]) -> tir::OpHandle {
    context.register_operation(&GAMMA);
    let i32_ty = builtin::IntegerType::new(context, 32);
    let predicate = ops::constant(
        context,
        0,
        i32_ty,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    let input = ops::constant(
        context,
        5,
        i32_ty,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    let arms = arm_results
        .iter()
        .map(|results| nodes_region(context, &[i32_ty], results))
        .collect();
    r#gamma(
        context,
        predicate.value_results()[0],
        vec![input.value_results()[0]],
        vec![i32_ty],
        arms,
        Default::default(),
        tir::InsertionPoint::Root,
    )
}

#[test]
fn a_gamma_binding_forwards_inputs_and_joins_arm_results() {
    let context = Context::with_default_dialects();
    let op = build_gamma(&context, &[&[(1, 32)], &[(2, 32)]]);

    let gamma = op
        .clone()
        .as_interface::<dyn Gamma>()
        .expect("gamma interface");
    assert_eq!(
        gamma.binding(),
        Binding {
            operands: 1..2,
            ports: 0..1,
            continue_: 0..0,
            exit: 0..1,
            results: 0..1,
        }
    );
    assert_eq!(gamma.arms().len(), 2);
    assert_eq!(gamma.predicate(), op.operands()[0]);
    op.verify(&context).expect("an aligned gamma verifies");
}

fn counted(
    context: &Context,
    predicate: tir::attributes::Predicate,
    shape: impl Fn(&[ValueId; 5]) -> Vec<ValueId>,
) -> tir::OpHandle {
    let i32_ty = builtin::IntegerType::new(context, 32);
    let i1 = builtin::IntegerType::new(context, 1);
    let bounds: Vec<ValueId> = [0, 10, 1, 7]
        .iter()
        .map(|&value| {
            ops::constant(
                context,
                value,
                i32_ty,
                Default::default(),
                tir::InsertionPoint::Root,
            )
            .value_results()[0]
        })
        .collect();
    let counter = context.create_value(i32_ty, None);
    let carried = context.create_value(i32_ty, None);
    let compare = tir::builtin::r#cmpi(
        context,
        counter.id(),
        bounds[1],
        tir::attributes::AttributeValue::Predicate(predicate),
        i1,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    let advance = tir::builtin::r#addi(
        context,
        counter.id(),
        bounds[2],
        i32_ty,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    let doubled = ops::addi(
        context,
        carried.id(),
        carried.id(),
        i32_ty,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    let results = shape(&[
        counter.id(),
        compare.value_results()[0],
        advance.value_results()[0],
        carried.id(),
        doubled.value_results()[0],
    ]);
    let body = context.create_nodes_region(
        vec![counter, carried],
        vec![compare.id(), advance.id(), doubled.id()],
        results,
    );
    tir::scf::r#for(
        context,
        bounds[0],
        vec![bounds[3]],
        bounds[1],
        bounds[2],
        vec![i32_ty, i32_ty],
        Some(body.id()),
        Default::default(),
        tir::InsertionPoint::Root,
    )
}

#[test]
fn a_loop_names_each_carried_value_on_every_side() {
    let context = Context::with_default_dialects();
    let op = counted(
        &context,
        tir::attributes::Predicate::Slt,
        |[i, cmp, next, a, doubled]| vec![*cmp, *next, *doubled, *i, *a],
    );
    let handle = &op;
    let body = context.get_region(
        op.clone()
            .as_interface::<dyn Theta>()
            .expect("for is theta")
            .body(),
    );
    let (ports, results) = (body.ports(), body.results());

    let carried = tir::binding::carried(&context, handle);

    let side = |index: usize| tir::binding::Carried {
        init: handle.operands()[index],
        port: ports[index].id(),
        next: results[1 + index],
        exit: results[3 + index],
        result: handle.results()[index],
    };
    assert_eq!(carried, vec![side(0), side(1)]);
}

#[test]
fn a_counted_loop_pins_its_comparison_increment_and_exits() {
    let context = Context::with_default_dialects();
    let op = counted(
        &context,
        tir::attributes::Predicate::Slt,
        |[i, cmp, next, a, doubled]| vec![*cmp, *next, *doubled, *i, *a],
    );

    op.verify(&context).expect("the pinned shape verifies");
    assert_eq!(
        op.as_interface::<dyn tir::CountedLoop>()
            .expect("for is counted")
            .induction(),
        Some(0)
    );
}

fn counted_error(
    predicate: tir::attributes::Predicate,
    shape: impl Fn(&[ValueId; 5]) -> Vec<ValueId>,
) -> String {
    let context = Context::with_default_dialects();
    let op = counted(&context, predicate, shape);
    op.verify(&context)
        .expect_err("the loop is not counted")
        .to_string()
}

#[test]
fn a_counted_loop_compares_signed_less_than() {
    let error = counted_error(
        tir::attributes::Predicate::Sgt,
        |[i, cmp, next, a, doubled]| vec![*cmp, *next, *doubled, *i, *a],
    );
    assert!(error.contains("cmpi slt"), "{error}");
}

#[test]
fn a_counted_loop_advances_its_counter_by_the_step() {
    let error = counted_error(tir::attributes::Predicate::Slt, |[i, cmp, _, a, doubled]| {
        vec![*cmp, *doubled, *doubled, *i, *a]
    });
    assert!(error.contains("advance"), "{error}");
}

#[test]
fn a_counted_loop_leaves_every_port_unchanged() {
    let error = counted_error(
        tir::attributes::Predicate::Slt,
        |[i, cmp, next, _, doubled]| vec![*cmp, *next, *doubled, *i, *doubled],
    );
    assert!(error.contains("exit value 1 must be port 1"), "{error}");
}
