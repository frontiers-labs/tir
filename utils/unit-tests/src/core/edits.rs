//! Placement-free edits on unordered regions: growing declared ports and
//! placing an op by its operands.

use tir::{
    builtin::{self, ops, ModuleOp},
    interp,
    scf::{LoopOp, SwitchOp},
    Context, ExitTarget, NonLocalExit, OpId, Operation, RegionId, Terminator, ValueId,
};

use super::fixtures::{self, module_ops};

fn function(context: &Context, module: &ModuleOp) -> OpId {
    module_ops(context, module.id())[0]
}

fn body_of(context: &Context, function: OpId) -> RegionId {
    context.get_op(function).regions()[0]
}

fn find<T: Operation>(context: &Context, region: RegionId) -> OpId {
    context
        .get_region(region)
        .op_ids()
        .into_iter()
        .find(|&op| context.get_op(op).is::<T>())
        .expect("the region holds the op")
}

fn run(context: &Context, function: OpId, args: &[i64]) -> i64 {
    let args = args
        .iter()
        .map(|&value| interp::Value::Int(tir::utils::APInt::new_signed(32, value)))
        .collect();
    interp::run_function(context, function, args).expect("runs")[0]
        .to_i64()
        .expect("an integer")
}

const LOOP: &str = r#"module {
  %fn_main = func.func @main(%0: !i32) -> !i32 {
    %1 = constant {value = 0} : !i1
    %2 = scf.loop (%3 = %0) {
      -> %1, %3, %3
    }
    -> %2
  }
  module_end
}"#;

#[test]
fn growing_a_loop_port_extends_every_aligned_range() {
    let (context, module) = fixtures::parse(LOOP);
    let function = function(&context, &module);
    let body = body_of(&context, function);
    let loop_op = find::<LoopOp>(&context, body);
    let init = context.get_region(body).ports()[0].id();
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let mut carried = None;
    let result = context.grow_port(loop_op, i32_ty, Some(init), |_, port| {
        carried = port;
        port
    });

    let grown = context.get_op(loop_op);
    let loop_body = context.get_region(grown.regions()[0]);
    assert_eq!(grown.value_operands().as_slice(), [init, init]);
    assert_eq!(loop_body.value_arguments().len(), 2);
    assert_eq!(loop_body.value_arguments()[1].id(), carried.unwrap());
    assert_eq!(loop_body.value_arguments()[1].ty(), i32_ty);
    let results = loop_body.value_results();
    assert_eq!(results.len(), 5, "predicate, two continue, two exit");
    assert_eq!(results[2], carried.unwrap(), "the port continues as itself");
    assert_eq!(
        results[4],
        carried.unwrap(),
        "the exit value defaults to the port"
    );
    assert_eq!(
        grown.value_results().as_slice(),
        [grown.value_results()[0], result]
    );
    tir::verify_op_tree(&context, module.id()).expect("the grown loop verifies");
}

#[test]
fn growing_a_loop_dependency_port_keeps_the_dependency_shape() {
    let (context, module) = fixtures::parse(LOOP);
    let function = function(&context, &module);
    let body = body_of(&context, function);
    let loop_op = find::<LoopOp>(&context, body);
    let chain = context.create_state();

    let result = context.grow_dep_port(loop_op, Some(chain), |_, port| port);

    let grown = context.get_op(loop_op);
    let loop_body = context.get_region(grown.regions()[0]);
    assert_eq!(grown.dep_operands().as_slice(), [chain]);
    assert_eq!(loop_body.dep_arguments().len(), 1);
    assert_eq!(loop_body.dep_results().len(), 2);
    assert_eq!(grown.dep_results().as_slice(), [result]);
    assert_eq!(
        loop_body.value_results().len(),
        3,
        "the value ranges are untouched"
    );
    tir::verify_op_tree(&context, module.id()).expect("the grown loop verifies");
}

const SWITCH: &str = r#"module {
  %fn_main = func.func @main(%0: !i32, %1: !i32) -> !i32 {
    scf.switch %0 {
      ->
    }
    {
      ->
    }
    -> %1
  }
  module_end
}"#;

#[test]
fn growing_a_gamma_port_forwards_to_and_joins_every_arm() {
    let (context, module) = fixtures::parse(SWITCH);
    let function = function(&context, &module);
    let body = body_of(&context, function);
    let switch = find::<SwitchOp>(&context, body);
    let input = context.get_region(body).ports()[1].id();
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let result = context.grow_port(switch, i32_ty, Some(input), |_, port| port);

    let grown = context.get_op(switch);
    assert_eq!(
        grown.value_operands().as_slice(),
        [context.get_region(body).ports()[0].id(), input]
    );
    for arm in grown.regions() {
        let arm = context.get_region(arm);
        assert_eq!(arm.value_arguments().len(), 1);
        assert_eq!(arm.value_results(), vec![arm.value_arguments()[0].id()]);
    }
    assert_eq!(grown.value_results().as_slice(), [result]);
    tir::verify_op_tree(&context, module.id()).expect("the grown gamma verifies");
}

tir::helpers::operation! {
    ExitOp {
        name: "exit",
        dialect: "test",
        format: "custom",
        operands: O {
            values: "*tir::Any",
        },
        interfaces: [Terminator, NonLocalExit],
    }
}

impl ExitOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.write("test.exit\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, tir::Error)> {
        let _ = parser;
        Ok(Box::new(ExitOpBuilder::new(context).build()))
    }
}

impl Terminator for ExitOp {}

impl NonLocalExit for ExitOp {
    fn target(&self) -> ExitTarget {
        ExitTarget::InnermostLoop
    }

    fn values(&self) -> Vec<ValueId> {
        self.operands().to_vec()
    }
}

/// `LOOP` with a conditional in the body whose taken arm leaves through
/// `test.exit`.
fn loop_with_exit() -> (Context, OpId, OpId) {
    let (context, module) = fixtures::parse(LOOP);
    ExitOp::register_interfaces(&context);
    let function = function(&context, &module);
    let loop_op = find::<LoopOp>(&context, body_of(&context, function));
    let body = context.get_op(loop_op).regions()[0];
    let predicate = context.get_region(body).results()[0];
    let exit = ExitOpBuilder::new(&context).values(vec![]).build();
    let arm = |ops: Vec<OpId>| context.create_nodes_region(vec![], 0, ops, vec![], 0).id();
    let conditional = tir::scf::SwitchOpBuilder::new(&context)
        .predicate(predicate)
        .inputs(vec![])
        .arms(vec![arm(vec![]), arm(vec![exit.id()])])
        .result_types(vec![])
        .build();
    context.add(body, conditional.id());
    (context, loop_op, exit.id())
}

#[test]
fn a_non_local_exit_leaving_the_loop_resolves_to_it() {
    let (context, loop_op, exit) = loop_with_exit();

    assert_eq!(
        tir::analysis::exits::resolve_exit_target(&context, exit).ok(),
        Some(loop_op)
    );
}

#[test]
fn growing_a_loop_port_feeds_the_exits_leaving_it() {
    let (context, loop_op, exit) = loop_with_exit();
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let init = context.get_op(loop_op).value_operands()[0];

    context.grow_port(loop_op, i32_ty, Some(init), |_, port| port);

    let port = context
        .get_region(context.get_op(loop_op).regions()[0])
        .value_arguments()[1]
        .id();
    assert_eq!(context.get_op(exit).operands().as_slice(), [port]);
}

const NESTED: &str = r#"module {
  %fn_main = func.func @main(%0: !i32) -> !i32 {
    %1 = constant {value = 0} : !i1
    %2 = scf.loop (%3 = %0) {
      %4 = addi %3, %0 : !i32
      -> %1, %4, %3
    }
    -> %2
  }
  module_end
}"#;

#[test]
fn add_auto_places_an_op_where_its_operands_meet() {
    let (context, module) = fixtures::parse(NESTED);
    let function = function(&context, &module);
    let outer = body_of(&context, function);
    let inner = context.get_op(find::<LoopOp>(&context, outer)).regions()[0];
    let argument = context.get_region(outer).ports()[0].id();
    let port = context.get_region(inner).ports()[0].id();
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let mixed = ops::addi(&context, port, argument, i32_ty).build();
    let outside = ops::addi(&context, argument, argument, i32_ty).build();

    assert_eq!(context.add_auto(mixed.id()), inner);
    assert_eq!(context.add_auto(outside.id()), outer);
    assert_eq!(context.parent_nodes_region(mixed.id()), Some(inner));
}

#[test]
fn add_auto_pins_an_op_to_its_dependency() {
    let (context, module) = fixtures::parse(NESTED);
    let function = function(&context, &module);
    let outer = body_of(&context, function);
    let loop_op = find::<LoopOp>(&context, outer);
    let chain = context.grow_dep_port(loop_op, Some(context.create_state()), |_, port| port);
    let inner = context.get_op(loop_op).regions()[0];
    let inner_chain = context.get_region(inner).dep_arguments()[0].id();
    let argument = context.get_region(outer).ports()[0].id();
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let pinned_out = ops::addi(&context, argument, argument, i32_ty)
        .dep_operand(chain)
        .build();
    let pinned_in = ops::addi(&context, argument, argument, i32_ty)
        .dep_operand(inner_chain)
        .build();

    assert_eq!(context.add_auto(pinned_out.id()), outer);
    assert_eq!(context.add_auto(pinned_in.id()), inner);
}

const COUNTED: &str = r#"module {
  %fn_main = func.func @main(%0: !i32, %1: !i32, %2: !i32) -> !i32 {
    %3 = scf.for %4 = %0 to %1 step %2 {
      ->
    }
    -> %3
  }
  module_end
}"#;

#[test]
fn growing_a_counted_loop_port_lands_before_its_bounds() {
    let (context, module) = fixtures::parse(COUNTED);
    let function = function(&context, &module);
    let body = body_of(&context, function);
    let for_op = find::<tir::scf::ForOp>(&context, body);
    let ports = context.get_region(body).ports();
    let (lb, ub, step) = (ports[0].id(), ports[1].id(), ports[2].id());
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let result = context.grow_port(for_op, i32_ty, Some(ub), |_, port| port);

    let grown = context.get_op(for_op);
    assert_eq!(grown.value_operands().as_slice(), [lb, ub, ub, step]);
    assert_eq!(grown.value_results().len(), 2);
    assert_eq!(grown.value_results()[1], result);
    tir::verify_op_tree(&context, module.id()).expect("the grown counted loop verifies");
    assert_eq!(run(&context, function, &[0, 3, 1]), 3);
}
