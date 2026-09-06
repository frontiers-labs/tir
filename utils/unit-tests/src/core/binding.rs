//! `binds:` on a fixture op: the macro derives the binding, and the verifier
//! holds every aligned range to one length and one type per offset.

use tir::{
    builtin::{self, ops},
    Binding, Context, Gamma, Operation, RegionId, Theta, TypeId, ValueId,
};

tir::helpers::operation! {
    ThetaOp {
        name: "theta",
        dialect: "test",
        operands: O {
            inits: "*tir::Any",
        },
        results: R {
            results: "*tir::Any",
        },
        regions: R {
            body: Region {
                kind: Nodes,
            }
        },
        binds: Theta {
            carried: inits ~ body.ports ~ body.results[1..n+1] ~ body.results[n+1..] ~ results,
            predicate: body.results[0],
        },
    }
}

tir::helpers::operation! {
    GammaOp {
        name: "gamma",
        dialect: "test",
        operands: O {
            predicate: "tir::builtin::IntegerType",
            inputs: "*tir::Any",
        },
        results: R {
            results: "*tir::Any",
        },
        regions: R {
            arms: Region {
                kind: Nodes,
                variadic: true,
            }
        },
        binds: Gamma {
            predicate,
            forwarded: inputs ~ arms.ports,
            joined: arms.results ~ results,
        },
    }
}

/// An unordered region over `port_types`, producing one constant per entry of
/// `result_values` (a `(value, width)` pair each).
fn nodes_region(context: &Context, port_types: &[TypeId], results: &[(i64, u32)]) -> RegionId {
    let ports = port_types
        .iter()
        .map(|&ty| context.create_value(ty, None))
        .collect();
    let constants: Vec<_> = results
        .iter()
        .map(|&(value, width)| {
            ops::constant(context, value, builtin::IntegerType::new(context, width)).build()
        })
        .collect();
    let values: Vec<ValueId> = constants.iter().map(|c| c.result()).collect();
    let op_ids = constants.iter().map(|c| c.id()).collect();
    context
        .create_nodes_region(ports, 0, op_ids, values, 0)
        .id()
}

fn build_theta(context: &Context, port_types: &[TypeId], results: &[(i64, u32)]) -> ThetaOp {
    let i32_ty = builtin::IntegerType::new(context, 32);
    let init = ops::constant(context, 7, i32_ty).build();
    ThetaOpBuilder::new(context)
        .inits(vec![init.result()])
        .body(nodes_region(context, port_types, results))
        .result_types(vec![i32_ty])
        .build()
}

#[test]
fn a_theta_binding_aligns_inits_ports_continue_exit_and_results() {
    let context = Context::with_default_dialects();
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let op = build_theta(&context, &[i32_ty], &[(1, 1), (2, 32), (3, 32)]);

    assert_eq!(
        op.carried(),
        Binding {
            operands: 0..1,
            ports: 0..1,
            continue_: 1..2,
            exit: 2..3,
            results: 0..1,
        }
    );
    let body = context.get_region(Theta::body(&op));
    assert_eq!(op.predicate(), body.value_results()[0]);
    op.verify(&context).expect("an aligned theta verifies");
}

#[test]
fn a_theta_with_a_short_result_list_is_rejected() {
    let context = Context::with_default_dialects();
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let op = build_theta(&context, &[i32_ty], &[(1, 1), (2, 32)]);

    let error = op.verify(&context).expect_err("exit values are missing");
    assert!(error.to_string().contains("exit"), "{error}");
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

#[test]
fn a_theta_predicate_must_be_a_boolean() {
    let context = Context::with_default_dialects();
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let op = build_theta(&context, &[i32_ty], &[(1, 32), (2, 32), (3, 32)]);

    let error = op.verify(&context).expect_err("the predicate is not i1");
    assert!(error.to_string().contains("predicate"), "{error}");
}

fn build_gamma(context: &Context, arm_results: &[&[(i64, u32)]]) -> GammaOp {
    let i32_ty = builtin::IntegerType::new(context, 32);
    let predicate = ops::constant(context, 0, i32_ty).build();
    let input = ops::constant(context, 5, i32_ty).build();
    let arms = arm_results
        .iter()
        .map(|results| nodes_region(context, &[i32_ty], results))
        .collect();
    GammaOpBuilder::new(context)
        .predicate(predicate.result())
        .inputs(vec![input.result()])
        .arms(arms)
        .result_types(vec![i32_ty])
        .build()
}

#[test]
fn a_gamma_binding_forwards_inputs_and_joins_arm_results() {
    let context = Context::with_default_dialects();
    let op = build_gamma(&context, &[&[(1, 32)], &[(2, 32)]]);

    assert_eq!(
        op.forwarded(),
        Binding {
            operands: 1..2,
            ports: 0..1,
            continue_: 0..0,
            exit: 0..1,
            results: 0..1,
        }
    );
    assert_eq!(op.arms().len(), 2);
    assert_eq!(Gamma::predicate(&op), op.operands()[0]);
    op.verify(&context).expect("an aligned gamma verifies");
}

#[test]
fn every_gamma_arm_must_produce_the_op_results() {
    let context = Context::with_default_dialects();
    let op = build_gamma(&context, &[&[(1, 32)], &[(2, 32), (3, 32)]]);

    let error = op
        .verify(&context)
        .expect_err("the second arm yields too much");
    assert!(error.to_string().contains("arm 1"), "{error}");
}

/// `scf.r#for %i = %lb to %ub step %s (%a = %init)` built by hand, with the body
/// results `shape` chooses from the counter, its comparison, its increment,
/// the carried port and its doubled value.
fn counted(
    context: &Context,
    predicate: tir::attributes::Predicate,
    shape: impl Fn(&[ValueId; 5]) -> Vec<ValueId>,
) -> tir::scf::ForOp {
    use tir::builtin::{AddIOpBuilder, CmpIOpBuilder};
    let i32_ty = builtin::IntegerType::new(context, 32);
    let i1 = builtin::IntegerType::new(context, 1);
    let bounds: Vec<ValueId> = [0, 10, 1, 7]
        .iter()
        .map(|&value| ops::constant(context, value, i32_ty).build().result())
        .collect();
    let counter = context.create_value(i32_ty, None);
    let carried = context.create_value(i32_ty, None);
    let compare = CmpIOpBuilder::new(context)
        .lhs(counter.id())
        .rhs(bounds[1])
        .predicate(predicate)
        .result_type(i1)
        .build();
    let advance = AddIOpBuilder::new(context)
        .lhs(counter.id())
        .rhs(bounds[2])
        .result_type(i32_ty)
        .build();
    let doubled = ops::addi(context, carried.id(), carried.id(), i32_ty).build();
    let results = shape(&[
        counter.id(),
        compare.result(),
        advance.result(),
        carried.id(),
        doubled.result(),
    ]);
    let body = context.create_nodes_region(
        vec![counter, carried],
        0,
        vec![compare.id(), advance.id(), doubled.id()],
        results,
        0,
    );
    tir::scf::ForOpBuilder::new(context)
        .lb(bounds[0])
        .inits(vec![bounds[3]])
        .ub(bounds[1])
        .step(bounds[2])
        .body(body.id())
        .result_types(vec![i32_ty, i32_ty])
        .build()
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
    assert_eq!(tir::CountedLoop::induction(&op), Some(0));
}

#[test]
fn a_counted_loop_compares_signed_less_than() {
    let context = Context::with_default_dialects();
    let op = counted(
        &context,
        tir::attributes::Predicate::Sgt,
        |[i, cmp, next, a, doubled]| vec![*cmp, *next, *doubled, *i, *a],
    );

    let error = op.verify(&context).expect_err("the comparison is not slt");
    assert!(error.to_string().contains("cmpi slt"), "{error}");
}

#[test]
fn a_counted_loop_advances_its_counter_by_the_step() {
    let context = Context::with_default_dialects();
    let op = counted(
        &context,
        tir::attributes::Predicate::Slt,
        |[i, cmp, _, a, doubled]| vec![*cmp, *doubled, *doubled, *i, *a],
    );

    let error = op
        .verify(&context)
        .expect_err("the counter does not advance");
    assert!(error.to_string().contains("advance"), "{error}");
}

#[test]
fn a_counted_loop_leaves_every_port_unchanged() {
    let context = Context::with_default_dialects();
    let op = counted(
        &context,
        tir::attributes::Predicate::Slt,
        |[i, cmp, next, _, doubled]| vec![*cmp, *next, *doubled, *i, *doubled],
    );

    let error = op.verify(&context).expect_err("port 1 leaves changed");
    assert!(
        error.to_string().contains("exit value 1 must be port 1"),
        "{error}"
    );
}
