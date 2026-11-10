use tir::{
    builtin::MODULE,
    interp::{self, Value},
    parse::ir::parse_operation,
    passes::EqSatPass,
    utils::APInt,
    AnalysisManager, Context, NonLocalExit, Operation, Pass,
};

fn fixture(
    width: u32,
    constant: impl std::fmt::Display,
    attributes: &str,
) -> (Context, tir::OpHandle, tir::OpId, tir::OpId, tir::ValueId) {
    let source = format!(
        r#"module {{
  %f = func.func @f(%x: !i{width}) -> !i{width} {{
    %one = constant {{value = {constant}}} : !i{width}
    %y = muli %x, %one {attributes} : !i{width}
    func.return %y
  }}
  module_end
}}"#
    );
    parse(&source)
}

fn parse(source: &str) -> (Context, tir::OpHandle, tir::OpId, tir::OpId, tir::ValueId) {
    let context = Context::with_default_dialects();
    let module = parse_operation(&context, source, &MODULE).expect("input parses");
    tir::verify_op_tree(&context, module.id()).expect("input verifies");
    let function = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()[0];
    let body = context.get_region(context.get_op(function).regions()[0]);
    let block = body.iter(context.clone()).next().expect("entry block");
    let input = block.arguments()[0].id();
    let ret = *block.op_ids().last().expect("return");
    (context, module, function, ret, input)
}

fn execute(context: &Context, function: tir::OpId, width: u32, input: i64) -> APInt {
    let mut results = interp::run_function(
        context,
        function,
        vec![Value::Int(APInt::new_signed(u64::from(width), input))],
    )
    .expect("function executes");
    let Value::Int(result) = results.remove(0) else {
        panic!("expected integer result")
    };
    result.with_signed(false)
}

fn assert_complement_results(
    context: &Context,
    function: tir::OpId,
    width: u32,
    ordering: bool,
    reverse: bool,
) {
    for (a, b) in [(-1, 0), (0, 0), (1, 0)] {
        let taken = (if ordering { a < b } else { a == b }) != reverse;
        for k in [Some(0), Some(1), None] {
            for m in [Some(0), Some(1), None] {
                let args = vec![
                    Value::Int(APInt::new_signed(width.into(), a)),
                    Value::Int(APInt::new_signed(width.into(), b)),
                    k.map_or(Value::Poison, |n| Value::Int(APInt::new(1, n))),
                    m.map_or(Value::Poison, |n| Value::Int(APInt::new(1, n))),
                ];
                let actual = interp::run_function(context, function, args).unwrap();
                let expected = if taken { k } else { m.map(|n| 1 ^ n) };
                match (actual.as_slice(), expected) {
                    ([Value::Int(actual)], Some(expected)) => {
                        assert_eq!(actual.with_signed(false), APInt::new(1, expected))
                    }
                    ([Value::Poison], None) => {}
                    _ => panic!("wrong complement result {actual:?}"),
                }
            }
        }
    }
    for poisoned in [0, 1] {
        let mut args = vec![Value::Int(APInt::new(width.into(), 0)); 2];
        args.extend(vec![Value::Int(APInt::new(1, 0)); 2]);
        args[poisoned] = Value::Poison;
        assert!(matches!(
            interp::run_function(context, function, args),
            Err(interp::InterpError::Undefined(_))
        ));
    }
}

#[test]
fn comparison_complements_preserve_control_and_remove_the_covered_true_arm() {
    let original = include_str!("../../../../core/checks/Inputs/eqsat/cmp_complement.tir");
    for reverse in [false, true] {
        for width in [8u32, 32, 128] {
            let source = original.replace("!i32", &format!("!i{width}"));
            let source = if reverse {
                source
                    .replace("\"slt\"", "\"@\"")
                    .replace("\"sge\"", "\"slt\"")
                    .replace("\"@\"", "\"sge\"")
                    .replace("\"eq\"", "\"@\"")
                    .replace("\"ne\"", "\"eq\"")
                    .replace("\"@\"", "\"ne\"")
            } else {
                source
            };
            let context = Context::with_default_dialects();
            let module = parse_operation(&context, &source, &MODULE).unwrap();
            let functions = context
                .get_region(context.get_op(module.id()).regions()[0])
                .op_ids()
                .into_iter()
                .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
                .collect::<Vec<_>>();
            assert_eq!(functions.len(), 2);
            let mut pass = EqSatPass::new();
            for (index, &function) in functions.iter().enumerate() {
                let body = context.get_region(context.get_op(function).regions()[0]);
                let inputs = body.ports();
                let selector = body.op_ids()[0];
                let selector_attrs = context.get_op(selector).attributes();
                let gamma = body.op_ids()[1];
                let arms = context.get_op(gamma).regions();
                let false_arm = context.get_region(arms[0]);
                let true_arm = context.get_region(arms[1]);
                for round in 0..=2 {
                    if round > 0 {
                        pass.run(function, &context, &AnalysisManager::new())
                            .unwrap();
                        tir::verify_op_tree(&context, module.id()).unwrap();
                        assert_eq!(body.op_ids(), [selector, gamma]);
                        assert_eq!(context.get_op(selector).attributes(), selector_attrs);
                        assert_eq!(
                            context.get_op(gamma).value_operands()[0],
                            context.get_op(selector).results()[0]
                        );
                        assert!(
                            true_arm.op_ids().is_empty(),
                            "{}",
                            printed_ir(&context, &module)
                        );
                        assert_eq!(true_arm.results(), [inputs[2].id()]);
                        let ops = false_arm.op_ids();
                        assert_eq!(ops.len(), 2);
                        let one = context.get_op(ops[0]);
                        let xor = context.get_op(ops[1]);
                        assert!(one.is_operation(&tir::builtin::CONSTANT));
                        assert_eq!(
                            one.attr("value")
                                .unwrap()
                                .int_at(1)
                                .unwrap()
                                .with_signed(false),
                            APInt::new(1, 1)
                        );
                        assert!(one.operands().is_empty());
                        assert!(one.attr("predicate").is_none());
                        assert!(xor.is_operation(&tir::builtin::XORI));
                        assert_eq!(
                            xor.operands().as_slice(),
                            [one.results()[0], inputs[3].id()]
                        );
                        assert_eq!(false_arm.results().as_slice(), xor.results().as_slice());
                    }
                    assert_complement_results(&context, function, width, index == 0, reverse);
                }
            }
        }
    }
}

#[test]
fn comparison_complements_keep_uncovered_users_and_metadata() {
    let original = include_str!("../../../../core/checks/Inputs/eqsat/cmp_complement.tir");
    for case in [
        "predicate",
        "comparison-attribute",
        "xor-attribute",
        "shared",
        "metadata",
        "outside",
    ] {
        let source = match case {
            "predicate" => original
                .replace("\"sge\"", "\"sgt\"")
                .replace("\"ne\"", "\"eq\""),
            "comparison-attribute" => original.replace(
                "%t = cmpi %a, %b {predicate = ",
                "%t = cmpi %a, %b {retained = \"comparison\", predicate = ",
            ),
            "xor-attribute" => {
                original.replace("%y = xori %t, %k", "%y = xori %t, %k {retained = \"xor\"}")
            }
            "shared" => original.replace("-> %y", "%z = xori %t, %y : !i1\n      -> %z"),
            "outside" => original.replace(
                "%y = xori",
                "%nested = scf.switch %k { -> %t } { -> %k }\n      %y = xori",
            ),
            _ => original.to_owned(),
        };
        let context = Context::with_default_dialects();
        let module = parse_operation(&context, &source, &MODULE).unwrap();
        let functions = context
            .get_region(context.get_op(module.id()).regions()[0])
            .op_ids();
        for function in functions
            .into_iter()
            .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        {
            let body = context.get_region(context.get_op(function).regions()[0]);
            let gamma = context.get_op(body.op_ids()[1]);
            let arm = context.get_region(gamma.regions()[1]);
            let comparison = arm.op_ids()[0];
            let value = context.get_op(comparison).results()[0];
            if case == "metadata" {
                let mut attrs = context.get_op(function).attributes();
                attrs.push(
                    context
                        .named_attribute("retained", tir::attributes::AttributeValue::Value(value)),
                );
                context.set_op_attributes(function, attrs);
            }
            let comparison_attrs = context.get_op(comparison).attributes();
            let xor = arm
                .op_ids()
                .into_iter()
                .find(|&op| context.get_op(op).is_operation(&tir::builtin::XORI))
                .unwrap();
            let xor_attrs = context.get_op(xor).attributes();
            let args = [(-1, 0, 1, 0), (0, 0, 1, 1), (1, 0, 0, 1)];
            let execute = |(a, b, k, m)| {
                interp::run_function(
                    &context,
                    function,
                    vec![
                        Value::Int(APInt::new_signed(32, a)),
                        Value::Int(APInt::new_signed(32, b)),
                        Value::Int(APInt::new(1, k)),
                        Value::Int(APInt::new(1, m)),
                    ],
                )
                .unwrap()
                .into_iter()
                .map(|v| v.to_i64().unwrap())
                .collect::<Vec<_>>()
            };
            let before = args.map(execute);
            EqSatPass::new()
                .run(function, &context, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            assert_eq!(args.map(execute), before, "{case}");
            if case == "shared" {
                continue;
            }
            assert!(arm.op_ids().contains(&comparison), "{case}");
            assert!(arm.op_ids().contains(&xor), "{case}");
            assert_eq!(context.get_op(xor).attributes(), xor_attrs, "{case}");
            if case == "xor-attribute" {
                assert!(
                    context
                        .get_op(comparison)
                        .is_operation(&tir::builtin::CONSTANT),
                    "{case}"
                );
            } else {
                assert!(
                    context.get_op(comparison).is_operation(&tir::builtin::CMPI),
                    "{case}"
                );
                assert_eq!(
                    context.get_op(comparison).attributes(),
                    comparison_attrs,
                    "{case}"
                );
            }
            if case == "metadata" {
                assert_eq!(
                    context.get_op(function).attr("retained"),
                    Some(tir::attributes::AttributeValue::Value(value))
                );
            }
        }
    }
}

#[test]
fn constant_comparison_fixture_preserves_types_predicates_and_poison() {
    let context = Context::with_default_dialects();
    let module = parse_operation(
        &context,
        include_str!("../../../../core/checks/Inputs/eqsat/constant_fold_attrs.tir"),
        &MODULE,
    )
    .unwrap();
    let functions = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()
        .into_iter()
        .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        .collect::<Vec<_>>();
    assert_eq!(functions.len(), 7);
    let mut pass = EqSatPass::new();
    for (index, &function) in functions.iter().enumerate() {
        let body = context.get_region(context.get_op(function).regions()[0]);
        let original = body.op_ids();
        let comparison = original
            .iter()
            .copied()
            .find(|&op| context.get_op(op).is_operation(&tir::builtin::CMPI))
            .unwrap();
        let ty = context
            .get_value(context.get_op(comparison).results()[0])
            .ty();
        for round in 0..2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                let ret = *body.op_ids().last().unwrap();
                let result = context.get_op(ret).value_operands()[0];
                let op = context.get_op(context.get_value(result).defining_op().unwrap());
                assert_eq!(context.get_value(result).ty(), ty);
                if index < 3 {
                    assert!(body
                        .op_ids()
                        .iter()
                        .all(|&id| !context.get_op(id).is_operation(&tir::builtin::CMPI)));
                    assert!(op.is_operation(&tir::builtin::CONSTANT), "case {index}");
                    assert!(op.operands().is_empty());
                    assert!(op.attr("predicate").is_none());
                    let expected = u64::from(index != 1);
                    assert_eq!(
                        op.attr("value")
                            .unwrap()
                            .int_at(1)
                            .map(|n| n.with_signed(false)),
                        Some(APInt::new(1, expected))
                    );
                } else {
                    assert!(op.is_operation(&tir::builtin::CMPI), "case {index}");
                }
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            let cases: &[Option<u64>] = if index == 4 {
                &[Some(2), Some(4), None]
            } else {
                &[Some(0)]
            };
            for &input in cases {
                let args = if index == 4 {
                    vec![input.map_or(Value::Poison, |n| Value::Int(APInt::new(32, n)))]
                } else {
                    vec![]
                };
                let values = interp::run_function(&context, function, args).unwrap();
                let expected = match index {
                    0 | 2 | 6 => Some(1),
                    1 | 3 => Some(0),
                    4 => input.map(|n| u64::from(n < 3)),
                    5 => None,
                    _ => unreachable!(),
                };
                match (values.as_slice(), expected) {
                    ([Value::Int(value)], Some(n)) => {
                        assert_eq!(value.clone().with_signed(false), APInt::new(1, n))
                    }
                    ([Value::Poison], None) => {}
                    _ => panic!("unexpected result for case {index}, round {round}: {values:?}"),
                }
            }
        }
    }
}

#[test]
fn mul_one_publishes_uses_and_preserves_integer_and_poison_results() {
    let mut pass = EqSatPass::new();
    for width in [1, 8, 16, 32, 64, 128] {
        let (context, module, function, ret, input) = fixture(width, 1, "");
        let values = [-17, -1, 0, 1, 19];
        let before: Vec<_> = values
            .iter()
            .map(|&value| execute(&context, function, width, value))
            .collect();
        let poison =
            interp::run_function(&context, function, vec![Value::Poison]).expect("poison executes");
        assert!(matches!(poison.as_slice(), [Value::Poison]));
        pass.run(function, &context, &AnalysisManager::new())
            .expect("eqsat runs");
        assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
        tir::verify_op_tree(&context, module.id()).expect("published IR verifies");
        for (value, expected) in values.into_iter().zip(before) {
            assert_eq!(execute(&context, function, width, value), expected);
        }
        assert!(matches!(
            interp::run_function(&context, function, vec![Value::Poison])
                .expect("poison executes")
                .as_slice(),
            [Value::Poison]
        ));
        pass.run(function, &context, &AnalysisManager::new())
            .expect("a fresh revision runs again");
        assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
    }
}

#[test]
fn wide_interpreter_fixture_preserves_full_values_and_poison_under_default_eqsat() {
    let (context, module, _, _, _) = parse(include_str!(
        "../../../../core/checks/Interp/wide-constant.tir"
    ));
    let functions: Vec<_> = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()
        .into_iter()
        .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        .collect();
    let [main, shift, limbs] = functions.as_slice() else {
        panic!("expected main, shift and limbs")
    };
    let mut pass = EqSatPass::new();
    for round in 0..=2 {
        if round > 0 {
            for &function in &functions {
                pass.run(function, &context, &AnalysisManager::new())
                    .expect("default eqsat runs on each actual function");
            }
            tir::verify_op_tree(&context, module.id()).expect("published module verifies");
        }
        assert_eq!(
            execute(&context, *main, 200, 5),
            APInt::from_limbs(200, &[0, 3]),
            "main full-width result, round {round}"
        );
        assert_eq!(
            execute(&context, *limbs, 128, 5),
            APInt::from_limbs(128, &[10, 1]),
            "limbs full-width result, round {round}"
        );
        assert!(
            matches!(
                interp::run_function(&context, *shift, vec![])
                    .expect("shift executes")
                    .as_slice(),
                [Value::Poison]
            ),
            "shift by 2^64 is poison, round {round}"
        );
    }
}

#[test]
fn fp_inexact_fixture_preserves_unused_division_effects_under_default_eqsat() {
    let source = include_str!("../../../../core/checks/Interp/fp-optimized-inexact.tir");
    struct NoSpeculation;
    impl tir::Speculatable for NoSpeculation {
        fn is_speculatable(&self) -> bool {
            false
        }
    }
    for (unordered, speculative) in [(false, true), (true, true), (false, false)] {
        let source = if unordered {
            source.replace("func.return %flags state(%env3)", "-> %flags, %env3")
        } else {
            source.to_owned()
        };
        let source = source.replace(
            "    %unused, %env2 = fp.div",
            "    %ignored = fp.add %one, %three : !f32\n    %unused, %env2 = fp.div",
        );
        let context = Context::with_default_dialects();
        if !speculative {
            context.register_op_interface::<dyn tir::Speculatable>("fp", "add", |_| {
                tir::erase_op_interface::<dyn tir::Speculatable>(Box::new(NoSpeculation))
            });
        }
        let module = parse_operation(&context, &source, &MODULE).expect("FP fixture parses");
        tir::verify_op_tree(&context, module.id()).expect("FP source verifies");
        let function = context
            .get_region(context.get_op(module.id()).regions()[0])
            .op_ids()[0];
        let body = context.get_region(context.get_op(function).regions()[0]);
        let division = body
            .op_ids()
            .into_iter()
            .find(|&op| context.get_op(op).is_operation(&tir::fp::ops::DIV))
            .expect("flagged division");
        let state_input = context.get_op(division).state_operands();
        let state_output = context.get_op(division).state_results();
        let mut pass = EqSatPass::new();
        for round in 0..=2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .expect("default eqsat runs on FP fixture");
                tir::verify_op_tree(&context, module.id()).expect("published FP IR verifies");
            }
            assert_eq!(
                body.op_ids()
                    .iter()
                    .any(|&op| context.get_op(op).is_operation(&tir::fp::ops::ADD)),
                round == 0 || !speculative
            );
            for initial_flags in [0, 31] {
                let mut state = interp::ExecutionState::default();
                state.fp_environment.flags = initial_flags;
                let results =
                    interp::run_function_in_state(&context, function, vec![], &mut state, None)
                        .expect("FP fixture executes");
                let [Value::Int(flags)] = results.as_slice() else {
                    panic!("expected one i5 flag result")
                };
                assert_eq!(
                    flags.with_signed(false),
                    APInt::new(5, 1),
                    "returned flags, unordered {unordered}, initial {initial_flags}, round {round}"
                );
                assert_eq!(
                    state.fp_environment.flags, 1,
                    "status flags, unordered {unordered}, initial {initial_flags}, round {round}"
                );
            }
            assert!(body.op_ids().contains(&division));
            assert_eq!(context.get_op(division).state_operands(), state_input);
            assert_eq!(context.get_op(division).state_results(), state_output);
        }
    }
    use tir::utils::APFloat;
    let context = Context::with_default_dialects();
    let module = parse_operation(
        &context,
        include_str!("../../../../core/checks/EqSat/fp_arithmetic_identities_keep.tir"),
        &MODULE,
    )
    .unwrap();
    let functions = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()
        .into_iter()
        .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        .collect::<Vec<_>>();
    assert_eq!(functions.len(), 3);
    let before = printed_ir(&context, &module);
    let mut pass = EqSatPass::new();
    for round in 0..2 {
        if round > 0 {
            for &function in &functions {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
            }
        }
        tir::verify_op_tree(&context, module.id()).unwrap();
        assert_eq!(printed_ir(&context, &module), before);
        for (index, exp, mantissa, args) in [
            (0, 8, 23, vec![0x3f80_0000, 0x4b80_0000]),
            (0, 8, 23, vec![0x8000_0000, 0]),
            (
                1,
                11,
                52,
                vec![0x3ff0_0000_0000_0000, 0x4350_0000_0000_0000, 0],
            ),
        ] {
            let args = args
                .into_iter()
                .map(|bits| Value::Float(APFloat::from_bits(exp, mantissa, false, bits)))
                .collect();
            let actual = interp::run_function(&context, functions[index], args).unwrap();
            let [Value::Float(actual)] = actual.as_slice() else {
                panic!("expected float")
            };
            assert_eq!(
                actual.to_bits(),
                0,
                "rounded cancellation must not become an integer identity"
            );
        }
        for initial in [0, 1] {
            let mut state = interp::ExecutionState::default();
            state.fp_environment.flags = initial;
            let actual = interp::run_function_in_state(
                &context,
                functions[2],
                vec![Value::Float(APFloat::from_bits(8, 23, false, 0x7f80_0001))],
                &mut state,
                None,
            )
            .unwrap();
            let [Value::Float(actual)] = actual.as_slice() else {
                panic!("expected float")
            };
            assert_eq!(actual.to_bits(), 0x7fc0_0001);
            assert_eq!(state.fp_environment.flags, initial | 16);
        }
    }
}

#[test]
fn fp_parameter_identities_preserve_bits_poison_flags_and_remove_redundant_operations() {
    use tir::utils::APFloat;

    let mut unsimplified = Vec::new();
    for (name, expression, absolute) in [
        (
            "double neg",
            "%inner = fp.neg %x : FLOAT\n%result = fp.neg %inner : FLOAT",
            false,
        ),
        (
            "self copysign",
            "%result = fp.copysign %x, %x : FLOAT",
            false,
        ),
        (
            "repeated abs",
            "%inner = fp.abs %x : FLOAT\n%result = fp.abs %inner : FLOAT",
            true,
        ),
        (
            "abs neg",
            "%inner = fp.neg %x : FLOAT\n%result = fp.abs %inner : FLOAT",
            true,
        ),
    ] {
        for (width, exp, mantissa, sign, snan) in [
            (32, 8, 23, 0x8000_0000, 0x7fa1_2345),
            (64, 11, 52, 0x8000_0000_0000_0000, 0x7ff0_0000_0000_0042),
        ] {
            let expression = expression.replace("FLOAT", &format!("!f{width}"));
            let (context, module, function, ret, input) = parse(&format!(
                "module {{\n%f = func.func @f(%x: !f{width}) -> !f{width} {{\n{expression}\nfunc.return %result\n}}\nmodule_end\n}}"
            ));
            let body = context.get_region(context.get_op(function).regions()[0]);
            for round in 0..=1 {
                if round > 0 {
                    EqSatPass::new()
                        .run(function, &context, &AnalysisManager::new())
                        .unwrap();
                    tir::verify_op_tree(&context, module.id()).unwrap();
                }
                for initial in [0, 31] {
                    let mut state = interp::ExecutionState::default();
                    state.fp_environment.flags = initial;
                    for bits in [0, sign, snan, snan | sign] {
                        let actual = interp::run_function_in_state(
                            &context,
                            function,
                            vec![Value::Float(APFloat::from_bits(exp, mantissa, false, bits))],
                            &mut state,
                            None,
                        )
                        .unwrap();
                        let [Value::Float(actual)] = actual.as_slice() else {
                            panic!("expected float")
                        };
                        assert_eq!(
                            actual.to_bits(),
                            if absolute { bits & !sign } else { bits },
                            "{name}, f{width}, round {round}"
                        );
                        assert_eq!(state.fp_environment.flags, initial);
                    }
                    assert!(matches!(
                        interp::run_function_in_state(
                            &context,
                            function,
                            vec![Value::Poison],
                            &mut state,
                            None
                        )
                        .unwrap()
                        .as_slice(),
                        [Value::Poison]
                    ));
                    assert_eq!(state.fp_environment.flags, initial);
                }
            }
            let operations = body.op_ids();
            let simplified = if absolute {
                let abs = context.get_op(operations[0]);
                operations.len() == 2
                    && abs.is_operation(&tir::fp::ops::ABS)
                    && abs.operands().as_slice() == [input]
                    && context.get_op(ret).operands() == abs.results()
            } else {
                operations == [ret] && context.get_op(ret).operands().as_slice() == [input]
            };
            if !simplified {
                unsimplified.push((name, width, printed_ir(&context, &module)));
            }
        }
    }
    assert!(
        unsimplified.is_empty(),
        "redundant FP operations: {unsimplified:#?}"
    );
}

#[test]
fn fp_retained_constants_and_nested_dependencies_preserve_bits_poison_and_flags() {
    use tir::utils::APFloat;

    let (context, module, _, _, _) = parse(include_str!(
        "../../../../core/checks/Inputs/eqsat/fp_bit_identities.tir"
    ));
    let functions = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()
        .into_iter()
        .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        .collect::<Vec<_>>();
    assert_eq!(functions.len(), 10);
    let mut pass = EqSatPass::new();
    for (index, &function) in functions
        .iter()
        .enumerate()
        .filter(|(index, _)| matches!(index, 3 | 4 | 7..=9))
    {
        let body = context.get_region(context.get_op(function).regions()[0]);
        let inputs = body
            .iter(context.clone())
            .next()
            .unwrap()
            .arguments()
            .iter()
            .map(|arg| arg.id())
            .collect::<Vec<_>>();
        let original = body.op_ids();
        let ret = *original.last().unwrap();
        let (exp, mantissa, sign, snan) = if matches!(index, 3 | 8) {
            (11, 52, 0x8000_0000_0000_0000, 0x7ff0_0000_0000_0001)
        } else {
            (8, 23, 0x8000_0000, 0x7f80_0001)
        };
        let value = |bits| Value::Float(APFloat::from_bits(exp, mantissa, false, bits));
        for round in 0..=1 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&context, module.id()).unwrap();
            }
            let cases: &[u128] = if inputs.is_empty() {
                &[0]
            } else {
                &[0, sign, snan, snan | sign]
            };
            for initial in [0, 31] {
                for &bits in cases {
                    let args = if inputs.len() == 3 {
                        vec![value(bits), value(sign), value(snan | sign)]
                    } else {
                        inputs.iter().map(|_| value(bits)).collect()
                    };
                    let expected = match index {
                        3 | 4 => bits | sign,
                        7 => 0x8000_0000,
                        8 => 0x7ff0_0000_0000_0001,
                        9 if bits == sign => 0,
                        9 if bits & !sign == snan => bits | 0x0040_0000,
                        9 => bits,
                        _ => unreachable!(),
                    };
                    let mut state = interp::ExecutionState::default();
                    state.fp_environment.flags = initial;
                    let actual = interp::run_function_in_state(
                        &context,
                        function,
                        args.clone(),
                        &mut state,
                        None,
                    )
                    .unwrap();
                    let [Value::Float(actual)] = actual.as_slice() else {
                        panic!("expected float")
                    };
                    assert_eq!(
                        actual.to_bits(),
                        expected,
                        "function {index}, round {round}"
                    );
                    assert_eq!(state.fp_environment.flags, initial);
                    for poisoned in 0..args.len() {
                        let mut args = args.clone();
                        args[poisoned] = Value::Poison;
                        assert!(
                            matches!(
                                interp::run_function_in_state(
                                    &context, function, args, &mut state, None,
                                )
                                .unwrap()
                                .as_slice(),
                                [Value::Poison]
                            ),
                            "function {index}, poison {poisoned}, round {round}"
                        );
                        assert_eq!(state.fp_environment.flags, initial);
                    }
                }
            }
            if round > 0 && matches!(index, 7 | 8) {
                let operations = body.op_ids();
                assert_eq!(operations, [original[0], ret]);
                assert_eq!(
                    context.get_op(ret).operands(),
                    context.get_op(original[0]).results()
                );
            }
        }
    }
}

#[test]
fn retained_constant_placement_and_fixed_cost_preserve_source_identity() {
    use tir::utils::APFloat;

    const LIBRARY: &str =
        "rule expand: fp.abs(fp.neg(x)) => fp.abs(fp.abs(fp.abs(x))) proof trusted;";
    for (unordered, late, weight) in [
        (false, true, 1),
        (false, false, 1),
        (true, true, 1),
        (false, false, 0),
    ] {
        let constant = "%c = fp.constant {bits = 2147483648, retained = \"metadata\"} : !f32";
        let source = format!(
            "module {{\n%f = func.func @f(%x: !f32) -> !f32 {{\n{}\n%spare = fp.abs %x : !f32\n{}\n%neg = fp.neg %c : !f32\n%out = fp.abs %neg : !f32\n{}\n}}\nmodule_end\n}}",
            if late { "" } else { constant },
            if late { constant } else { "" },
            if unordered {
                "-> %out"
            } else {
                "func.return %out"
            },
        );
        let context = Context::with_default_dialects();
        let module = parse_operation(&context, &source, &MODULE).unwrap();
        let function = context
            .get_region(context.get_op(module.id()).regions()[0])
            .op_ids()[0];
        let body = context.get_region(context.get_op(function).regions()[0]);
        let original = body.op_ids();
        let constant = original
            .iter()
            .copied()
            .find(|&op| context.get_op(op).is_operation(&tir::fp::ops::CONSTANT))
            .unwrap();
        let value = context.get_op(constant).results()[0];
        let attributes = context.get_op(constant).attributes();
        let ty = context.get_value(value).ty();
        let before = printed_ir(&context, &module);
        for round in 0..=1 {
            if round > 0 {
                EqSatPass::with_library(tir_pdl::compile(LIBRARY).unwrap())
                    .unwrap()
                    .with_operation_cost(move |kind| match kind {
                        "fp.neg" => 10 * weight,
                        "fp.constant" => 1_000_000,
                        _ => weight,
                    })
                    .run(function, &context, &AnalysisManager::new())
                    .unwrap();
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            let mut state = interp::ExecutionState::default();
            state.fp_environment.flags = 31;
            let actual = interp::run_function_in_state(
                &context,
                function,
                vec![Value::Float(APFloat::from_bits(8, 23, false, 0x7f80_0001))],
                &mut state,
                None,
            )
            .unwrap();
            let [Value::Float(actual)] = actual.as_slice() else {
                panic!("expected float")
            };
            assert_eq!(actual.to_bits(), 0);
            assert_eq!(state.fp_environment.flags, 31);
            assert!(body.op_ids().contains(&constant));
            assert_eq!(context.get_op(constant).results().as_slice(), [value]);
            assert_eq!(context.get_op(constant).attributes(), attributes);
            assert_eq!(context.get_value(value).ty(), ty);
            assert_eq!(context.get_value(value).defining_op(), Some(constant));
        }
        let after = printed_ir(&context, &module);
        if (!unordered && late) || weight == 0 {
            assert_eq!(after, before);
        } else {
            assert!(!after.contains("fp.neg"), "{after}");
            assert_eq!(after.matches("fp.abs").count(), 3, "{after}");
        }
    }
}

#[test]
fn fp_copysign_neg_self_leaves_one_neg_and_preserves_bits_poison_and_flags() {
    use tir::utils::APFloat;

    let (context, module, _, _, _) = parse(include_str!(
        "../../../../core/checks/Inputs/eqsat/fp-copysign-neg-self.tir"
    ));
    let functions = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()
        .into_iter()
        .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        .collect::<Vec<_>>();
    assert_eq!(functions.len(), 4);
    let mut pass = EqSatPass::new();
    for (index, &function) in functions.iter().enumerate() {
        let body = context.get_region(context.get_op(function).regions()[0]);
        let copy = body
            .op_ids()
            .into_iter()
            .find(|&op| context.get_op(op).is_operation(&tir::fp::ops::COPYSIGN))
            .unwrap();
        let input = context.get_op(copy).operands()[0];
        let ret = *body.op_ids().last().unwrap();
        for round in 0..=1 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&context, module.id()).unwrap();
            }
            for initial in [0, 31] {
                let mut state = interp::ExecutionState::default();
                state.fp_environment.flags = initial;
                if index % 2 == 0 {
                    let (exp, mantissa, sign, snan) = if index == 0 {
                        (8, 23, 0x8000_0000, 0x7fa1_2345)
                    } else {
                        (11, 52, 0x8000_0000_0000_0000, 0x7ff0_0000_0000_0042)
                    };
                    for (bits, expected) in [(sign, 0), (snan, snan | sign)] {
                        let value = Value::Float(APFloat::from_bits(exp, mantissa, false, bits));
                        let actual = interp::run_function_in_state(
                            &context,
                            function,
                            vec![value.clone(), value],
                            &mut state,
                            None,
                        )
                        .unwrap();
                        let [Value::Float(actual)] = actual.as_slice() else {
                            panic!("expected float")
                        };
                        assert_eq!(actual.to_bits(), expected);
                    }
                    assert!(matches!(
                        interp::run_function_in_state(
                            &context,
                            function,
                            vec![Value::Poison, Value::Poison],
                            &mut state,
                            None,
                        )
                        .unwrap()
                        .as_slice(),
                        [Value::Poison]
                    ));
                } else {
                    let actual =
                        interp::run_function_in_state(&context, function, vec![], &mut state, None)
                            .unwrap();
                    let [Value::Int(actual)] = actual.as_slice() else {
                        panic!("expected bits")
                    };
                    let (width, expected) = if index == 1 {
                        (32, 4288750405)
                    } else {
                        (64, 0)
                    };
                    assert_eq!(actual.with_signed(false), APInt::new(width, expected));
                }
                assert_eq!(state.fp_environment.flags, initial);
            }
            if round > 0 && index % 2 == 0 {
                let operations = body.op_ids();
                assert_eq!(operations.len(), 2, "one neg and the return remain");
                let neg = context.get_op(operations[0]);
                assert!(neg.is_operation(&tir::fp::ops::NEG));
                assert_eq!(neg.operands().as_slice(), [input]);
                assert_eq!(context.get_op(ret).operands(), neg.results());
            }
        }
    }
}

#[test]
fn fp_copysign_sign_rules_preserve_bits_poison_flags_and_original_operands() {
    use tir::utils::APFloat;

    let mut unsimplified = Vec::new();
    for (name, source) in [
        (
            "abs magnitude",
            include_str!("../../../../core/checks/Inputs/eqsat/fp_copysign_abs_magnitude.tir"),
        ),
        (
            "neg magnitude",
            include_str!("../../../../core/checks/Inputs/eqsat/fp_copysign_neg_magnitude.tir"),
        ),
        (
            "neg sign",
            include_str!("../../../../core/checks/Inputs/eqsat/fp_neg_copysign_neg_sign.tir"),
        ),
    ] {
        let (context, module, _, _, _) = parse(source);
        let functions = context
            .get_region(context.get_op(module.id()).regions()[0])
            .op_ids()
            .into_iter()
            .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
            .collect::<Vec<_>>();
        assert_eq!(functions.len(), 4);
        let mut pass = EqSatPass::new();
        for (index, &function) in functions.iter().enumerate() {
            let body = context.get_region(context.get_op(function).regions()[0]);
            let block = body.iter(context.clone()).next().unwrap();
            let inputs = block
                .arguments()
                .iter()
                .map(|arg| arg.id())
                .collect::<Vec<_>>();
            let ret = *body.op_ids().last().unwrap();
            let (exp, mantissa, sign, snan, one) = if index < 2 {
                (8, 23, 0x8000_0000, 0x7fa1_2345, 0x3f80_0000)
            } else {
                (
                    11,
                    52,
                    0x8000_0000_0000_0000,
                    0x7ff0_0000_0000_0042,
                    0x3ff0_0000_0000_0000,
                )
            };
            let value = |bits| Value::Float(APFloat::from_bits(exp, mantissa, false, bits));
            for round in 0..=1 {
                if round > 0 {
                    pass.run(function, &context, &AnalysisManager::new())
                        .unwrap();
                    tir::verify_op_tree(&context, module.id()).unwrap();
                }
                for initial in [0, 31] {
                    let mut state = interp::ExecutionState::default();
                    state.fp_environment.flags = initial;
                    if index % 2 == 0 {
                        for (x, y) in [
                            (sign, 0),
                            (0, sign),
                            (snan, sign),
                            (snan | sign, snan),
                            (one, snan | sign),
                        ] {
                            let actual = interp::run_function_in_state(
                                &context,
                                function,
                                vec![value(x), value(y)],
                                &mut state,
                                None,
                            )
                            .unwrap();
                            let [Value::Float(actual)] = actual.as_slice() else {
                                panic!("expected float")
                            };
                            assert_eq!(
                                actual.to_bits(),
                                (x & !sign) | (y & sign),
                                "{name}, function {index}, round {round}"
                            );
                            assert_eq!(state.fp_environment.flags, initial);
                        }
                        for args in [
                            vec![Value::Poison, value(one)],
                            vec![value(one), Value::Poison],
                            vec![Value::Poison, Value::Poison],
                        ] {
                            assert!(
                                matches!(
                                    interp::run_function_in_state(
                                        &context, function, args, &mut state, None
                                    )
                                    .unwrap()
                                    .as_slice(),
                                    [Value::Poison]
                                ),
                                "{name}, function {index}, round {round}"
                            );
                            assert_eq!(state.fp_environment.flags, initial);
                        }
                    } else {
                        let actual = interp::run_function_in_state(
                            &context,
                            function,
                            vec![],
                            &mut state,
                            None,
                        )
                        .unwrap();
                        let [Value::Int(actual)] = actual.as_slice() else {
                            panic!("expected bits")
                        };
                        let (width, expected) = if index == 1 {
                            (32, 4288750405)
                        } else {
                            (64, 0)
                        };
                        assert_eq!(actual.with_signed(false), APInt::new(width, expected));
                        assert_eq!(state.fp_environment.flags, initial);
                    }
                }
                if round > 0 && index % 2 == 0 {
                    let operations = body.op_ids();
                    let copy = context.get_op(operations[0]);
                    if operations.len() != 2
                        || !copy.is_operation(&tir::fp::ops::COPYSIGN)
                        || copy.operands().as_slice() != inputs.as_slice()
                        || context.get_op(ret).operands() != copy.results()
                    {
                        unsimplified.push((name, index, printed_ir(&context, &module)));
                    }
                }
            }
        }
    }
    assert!(
        unsimplified.is_empty(),
        "expected only copysign(original x, original y) and return: {unsimplified:#?}"
    );
}

#[test]
fn fp_sign_elimination_requires_defined_sign_and_preserves_original_executions() {
    use tir::utils::APFloat;

    let mut unsimplified = Vec::new();
    for (name, source, expression) in [
        (
            "abs copysign",
            include_str!("../../../../core/checks/Inputs/eqsat/fp_abs_copysign.tir"),
            "%inner = fp.copysign %x, %y : FLOAT\n%result = fp.abs %inner : FLOAT",
        ),
        (
            "copysign abs sign",
            include_str!("../../../../core/checks/Inputs/eqsat/fp_copysign_abs_sign.tir"),
            "%inner = fp.abs %y : FLOAT\n%result = fp.copysign %x, %inner : FLOAT",
        ),
    ] {
        let positive = |width| {
            let expression = expression.replace("FLOAT", &format!("!f{width}"));
            let sign_bits = if width == 32 { 2147483648_u64 } else { 0 };
            format!(
                "module {{\n%f = func.func @f(%x: !f{width}) -> !f{width} {{\n%y = fp.constant {{bits = {sign_bits}}} : !f{width}\n{expression}\nfunc.return %result\n}}\nmodule_end\n}}"
            )
        };
        for (source, constant_width) in [
            (source.to_owned(), None),
            (positive(32), Some(32)),
            (positive(64), Some(64)),
        ] {
            let (context, module, _, _, _) = parse(&source);
            let functions = context
                .get_region(context.get_op(module.id()).regions()[0])
                .op_ids()
                .into_iter()
                .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
                .collect::<Vec<_>>();
            assert_eq!(
                functions.len(),
                if constant_width.is_some() { 1 } else { 4 }
            );
            let mut pass = EqSatPass::new();
            for (index, &function) in functions.iter().enumerate() {
                let width = constant_width.unwrap_or(if index < 2 { 32 } else { 64 });
                let (exp, mantissa, sign, snan) = if width == 32 {
                    (8, 23, 0x8000_0000, 0x7fa1_2345)
                } else {
                    (11, 52, 0x8000_0000_0000_0000, 0x7ff0_0000_0000_0042)
                };
                let value = |bits| Value::Float(APFloat::from_bits(exp, mantissa, false, bits));
                let body = context.get_region(context.get_op(function).regions()[0]);
                let block = body.iter(context.clone()).next().unwrap();
                let inputs = block
                    .arguments()
                    .iter()
                    .map(|arg| arg.id())
                    .collect::<Vec<_>>();
                let ret = *body.op_ids().last().unwrap();
                let literal = body
                    .op_ids()
                    .into_iter()
                    .find(|&op| context.get_op(op).is_operation(&tir::fp::ops::CONSTANT));
                for round in 0..=1 {
                    if round > 0 {
                        pass.run(function, &context, &AnalysisManager::new())
                            .unwrap();
                        tir::verify_op_tree(&context, module.id()).unwrap();
                    }
                    for initial in [0, 31] {
                        let mut state = interp::ExecutionState::default();
                        state.fp_environment.flags = initial;
                        let args = match inputs.len() {
                            0 => vec![],
                            1 => vec![value(snan | sign)],
                            2 => vec![value(snan | sign), value(sign)],
                            _ => unreachable!(),
                        };
                        let actual = interp::run_function_in_state(
                            &context,
                            function,
                            args.clone(),
                            &mut state,
                            None,
                        )
                        .unwrap();
                        if inputs.is_empty() {
                            let [Value::Int(actual)] = actual.as_slice() else {
                                panic!("expected bits")
                            };
                            assert_eq!(
                                actual.with_signed(false),
                                APInt::new(width, if width == 32 { 2141266757 } else { 0 })
                            );
                        } else {
                            let [Value::Float(actual)] = actual.as_slice() else {
                                panic!("expected float")
                            };
                            assert_eq!(actual.to_bits(), snan, "{name}, f{width}, round {round}");
                            // A discarded sign input must not turn poison into a defined result.
                            let poisoned = if inputs.len() == 2 { 1 } else { 0 };
                            let mut args = args;
                            args[poisoned] = Value::Poison;
                            assert!(
                                matches!(
                                    interp::run_function_in_state(
                                        &context, function, args, &mut state, None,
                                    )
                                    .unwrap()
                                    .as_slice(),
                                    [Value::Poison]
                                ),
                                "{name}, f{width}, round {round}"
                            );
                        }
                        assert_eq!(state.fp_environment.flags, initial);
                    }
                }
                if constant_width.is_some() {
                    let operations = body
                        .op_ids()
                        .into_iter()
                        .filter(|&op| !context.get_op(op).is_operation(&tir::fp::ops::CONSTANT))
                        .collect::<Vec<_>>();
                    let abs = context.get_op(operations[0]);
                    if body.op_ids().len() != 3
                        || !body.op_ids().contains(&literal.unwrap())
                        || operations.len() != 2
                        || !abs.is_operation(&tir::fp::ops::ABS)
                        || abs.operands().as_slice() != [inputs[0]]
                        || context.get_op(ret).operands() != abs.results()
                    {
                        unsimplified.push((name, width, printed_ir(&context, &module)));
                    }
                }
            }
        }
    }
    assert!(
        unsimplified.is_empty(),
        "expected only abs(original x) and return for defined signs: {unsimplified:#?}"
    );
}

#[test]
fn partial_library_operations_keep_unused_undefined_behavior() {
    let library = tir_pdl::compile(
        r#"
        rule zero: builtin.addi(x: int<W>, 0) => x proof trusted;
        rule unsigned: builtin.divui(x: int<W>, c: const<W>) => x
          where c == 1, defined(x) proof trusted;
        rule signed: builtin.divsi(x: int<W>, c: const<W>) => x
          where c == 1, c != -1, defined(x) proof trusted;
        "#,
    )
    .unwrap();
    for (operator, dividend, divisor) in [("divui", "%x", 0), ("divsi", "%min", -1)] {
        let (context, module, function, _, _) = parse(&format!(
            r#"module {{
              %f = func.func @f(%x: !i8) -> !i8 {{
                %min = constant {{value = -128}} : !i8
                %divisor = constant {{value = {divisor}}} : !i8
                %zero = constant {{value = 0}} : !i8
                %unused = {operator} {dividend}, %divisor : !i8
                %result = addi %x, %zero : !i8
                func.return %result
              }}
              module_end
            }}"#
        ));
        let mut pass = EqSatPass::with_library(library.clone()).unwrap();
        for round in 0..3 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            assert!(
                matches!(
                    interp::run_function(&context, function, vec![Value::Int(APInt::new(8, 7))]),
                    Err(interp::InterpError::Undefined(_))
                ),
                "{operator} precondition must still fail, round {round}"
            );
        }
    }
}

#[test]
fn nonidentity_and_attributed_operations_keep_their_uses() {
    for (constant, attributes) in [(-1, ""), (2, ""), (1, "{nsw = true}")] {
        let (context, module, function, ret, _) = fixture(32, constant, attributes);
        let old = context.get_op(ret).operands();
        let before = execute(&context, function, 32, -7);
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .expect("eqsat runs");
        assert_eq!(context.get_op(ret).operands(), old);
        assert_eq!(execute(&context, function, 32, -7), before);
        tir::verify_op_tree(&context, module.id()).expect("published IR verifies");
    }
    let (context, module, _, _, _) = parse(include_str!(
        "../../../../core/checks/EqSat/integer_rule_guards.tir"
    ));
    let functions = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()
        .into_iter()
        .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        .collect::<Vec<_>>();
    assert_eq!(functions.len(), 7);
    let before = printed_ir(&context, &module);
    let mut pass = EqSatPass::new();
    for round in 0..2 {
        if round > 0 {
            for &function in &functions {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
            }
        }
        tir::verify_op_tree(&context, module.id()).unwrap();
        assert_eq!(printed_ir(&context, &module), before);
        for (index, width, bits, amount, expected) in [
            (0, 8, 255, None, Some(127)),
            (1, 8, 255, None, Some(128)),
            (2, 8, 1, None, Some(128)),
            (3, 64, u64::MAX, Some(1), Some(0x7fff_ffff_ffff_ffff)),
            (3, 64, 17, Some(0), Some(17)),
            (4, 32, 17, None, None),
            (5, 32, 0x8000_0000, None, None),
            (5, 32, 7, None, Some(0xffff_fff9)),
            (6, 64, 7, None, None),
        ] {
            let mut args = vec![Value::Int(APInt::new(width, bits))];
            args.extend(amount.map(|amount| Value::Int(APInt::new(width, amount))));
            let actual = interp::run_function(&context, functions[index], args);
            if index == 6 {
                assert!(matches!(actual.unwrap().as_slice(), [Value::Poison]));
            } else if let Some(expected) = expected {
                let actual = actual.unwrap();
                let [Value::Int(actual)] = actual.as_slice() else {
                    panic!("expected integer result")
                };
                assert_eq!(
                    actual.clone().with_signed(false),
                    APInt::new(width, expected)
                );
            } else {
                assert!(actual.is_err(), "division precondition must still fail");
            }
        }
    }
}

#[test]
fn caller_owned_typed_library_rejects_unsupported_contracts() {
    for source in [
        "rule other: builtin.addi(x: int<W>, 0) => builtin.trunci(x, x, x);",
        "rule guarded: builtin.muli(x: int<W>, 1) => x where 1 == 1;",
        "rule checked: builtin.muli(x: int<W>, 1) => x proof smt;",
    ] {
        let library = tir_pdl::compile(source).expect("valid PDL");
        assert!(EqSatPass::with_library(library).is_err());
    }
    let library = tir_pdl::compile(
        "rule other: builtin.addi(x: int<W>, 0) => builtin.trunci(x) proof trusted;",
    )
    .unwrap();
    let mut pass = EqSatPass::with_library(library).expect("unary RHS syntax is supported");
    let (context, module, function, _, _) = fixture(32, 1, "");
    let before = printed_ir(&context, &module);
    assert!(pass
        .run(function, &context, &AnalysisManager::new())
        .is_err());
    assert_eq!(printed_ir(&context, &module), before);
    let library = tir_pdl::compile("rule own: builtin.muli(x: int<32>, 1) => x proof trusted;")
        .expect("valid PDL");
    let mut pass = EqSatPass::with_library(library).expect("supported typed library");
    let (context, _, function, ret, input) = fixture(32, 1, "");
    pass.run(function, &context, &AnalysisManager::new())
        .expect("caller library runs");
    assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
    let (context, _, function, ret, _) = fixture(64, 1, "");
    let before = context.get_op(ret).operands();
    pass.run(function, &context, &AnalysisManager::new())
        .expect("other width runs");
    assert_eq!(context.get_op(ret).operands(), before);
}

#[test]
fn integer_identities_match_declared_value_and_poison_semantics_at_checked_widths() {
    use tir::sem::{ProofOutcome, SemGraph, SmtOracle, SymPayload};
    use tir_symbolic::{
        lang::SymKind,
        sem::{con, copy_subgraph_with, op, sym, CopyAction},
    };

    for (operator, constant, repeated) in [
        ("muli", 1, false),
        ("addi", 0, false),
        ("subi", 0, false),
        ("ori", 0, false),
        ("xori", 0, false),
        ("shli", 0, false),
        ("shrui", 0, false),
        ("shrsi", 0, false),
        ("andi", 0, true),
        ("andi", -1, false),
        ("ori", 0, true),
    ] {
        for width in [1, 8, 16, 32, 64, 128] {
            let (context, _, function, _, _) = identity_fixture(operator, width, constant, "");
            let body = context.get_region(context.get_op(function).regions()[0]);
            let block = body.iter(context.clone()).next().expect("entry block");
            let operation = context.get_op(block.op_ids()[1]).as_dyn_op();
            let specialize = |declared: &SemGraph, root| {
                let mut lhs = SemGraph::new();
                copy_subgraph_with(
                    &mut lhs,
                    declared,
                    root,
                    &mut std::collections::HashMap::new(),
                    &mut |graph, node| {
                        if matches!(declared.get_leaf_data(node), Some(SymPayload::SymbolId(1))) {
                            CopyAction::Replace(if repeated {
                                sym(graph, 0)
                            } else if constant < 0 {
                                let zero = con(graph, 0, width);
                                let magnitude = con(graph, constant.unsigned_abs(), width);
                                op(graph, SymKind::Sub, &[zero, magnitude])
                            } else {
                                con(graph, constant as u64, width)
                            })
                        } else {
                            CopyAction::Keep
                        }
                    },
                );
                lhs
            };
            let mut declared = SemGraph::new();
            let root = operation
                .semantic_expr(&mut declared)
                .expect("declared value semantics");
            let lhs = specialize(&declared, root);
            let mut rhs = SemGraph::new();
            sym(&mut rhs, 0);
            assert_eq!(
                SmtOracle.equivalent_outcome(&lhs, &rhs, &[width]),
                ProofOutcome::Proven,
                "{operator}, width {width}"
            );
            assert!(operation.requires_expr(&mut SemGraph::new()).is_none());
            let mut poison = SemGraph::new();
            if let Some(root) = operation.poison_expr(&mut poison) {
                let lhs = specialize(&poison, root);
                let mut rhs = SemGraph::new();
                con(&mut rhs, 0, 1);
                assert_eq!(
                    SmtOracle.equivalent_outcome(&lhs, &rhs, &[width]),
                    ProofOutcome::Proven,
                    "{operator} adds no poison, width {width}"
                );
            }
        }
    }
}

fn identity_fixture(
    operator: &str,
    width: u32,
    constant: i64,
    attributes: &str,
) -> (Context, tir::OpHandle, tir::OpId, tir::OpId, tir::ValueId) {
    parse(&format!(
        r#"module {{
  %f = func.func @f(%x: !i{width}) -> !i{width} {{
    %zero = constant {{value = {constant}}} : !i{width}
    %y = {operator} %x, %zero {attributes} : !i{width}
    func.return %y
  }}
  module_end
}}"#
    ))
}

#[test]
fn default_integer_identities_publish_input_and_preserve_bits_and_poison() {
    assert_integer_identity_cases(&[
        ("addi", false, 0, false),
        ("addi", false, 0, true),
        ("subi", false, 0, false),
    ]);
}

#[test]
fn default_bitwise_identities_publish_input_and_preserve_bits_and_poison() {
    assert_integer_identity_cases(&[
        ("ori", false, 0, false),
        ("xori", false, 0, false),
        ("andi", true, 0, false),
        ("andi", false, -1, false),
        ("ori", true, 0, false),
    ]);
}

#[test]
fn default_shift_identities_publish_input_and_preserve_bits_and_poison() {
    assert_integer_identity_cases(&[
        ("shli", false, 0, false),
        ("shrui", false, 0, false),
        ("shrsi", false, 0, false),
    ]);
}

fn assert_integer_identity_cases(cases: &[(&str, bool, i64, bool)]) {
    for &(operator, repeated, neutral, constant_first) in cases {
        let mut pass = EqSatPass::new();
        for width in [1, 8, 16, 32, 64, 128] {
            for (constant, attributes) in [
                (neutral, ""),
                (if neutral == 0 { 1 } else { 0 }, ""),
                (neutral, "{nsw = true}"),
                (neutral, "{nuw = true}"),
            ] {
                let (context, module, function, ret, input) =
                    identity_fixture(operator, width, constant, attributes);
                if repeated || constant_first {
                    let operation = context
                        .get_value(context.get_op(ret).operands()[0])
                        .defining_op()
                        .unwrap();
                    if constant_first {
                        let constant = context.get_op(operation).operands()[1];
                        context.set_op_operand(operation, 0, constant);
                    }
                    context.set_op_operand(operation, 1, input);
                }
                let original = context.get_op(ret).operands();
                let identity = (repeated || constant == neutral) && attributes.is_empty();
                let expected_operand = [input];
                let values = [i64::MIN, -17, -1, 0, 1, 19, i64::MAX];
                for round in 0..2 {
                    if round > 0 {
                        pass.run(function, &context, &AnalysisManager::new())
                            .expect("default eqsat runs");
                        assert_eq!(
                            context.get_op(ret).operands().as_slice(),
                            if identity {
                                &expected_operand
                            } else {
                                original.as_slice()
                            },
                            "{operator}, i{width}, constant={constant}, attributes={attributes}"
                        );
                        {
                            let ir = printed_ir(&context, &module);
                            assert_eq!(ir.contains(&format!("{operator} ")), !identity, "{ir}");
                            eprintln!(
                                "{operator}-zero round={round}, {attributes}, c={constant}\n{ir}"
                            );
                        }
                    }
                    tir::verify_op_tree(&context, module.id()).expect("IR verifies");
                    for value in values {
                        if matches!(operator, "shli" | "shrui" | "shrsi")
                            && constant as u64 >= u64::from(width)
                        {
                            assert!(matches!(
                                interp::run_function(
                                    &context,
                                    function,
                                    vec![Value::Int(APInt::new_signed(u64::from(width), value))]
                                )
                                .unwrap()
                                .as_slice(),
                                [Value::Poison]
                            ));
                            continue;
                        }
                        let input_bits = APInt::new_signed(u64::from(width), value);
                        let expected = if repeated || constant == neutral {
                            input_bits
                        } else {
                            let rhs = APInt::new(u64::from(width), constant as u64);
                            match operator {
                                "addi" => input_bits.add(&rhs),
                                "subi" => input_bits.sub(&rhs),
                                "ori" => input_bits.or(&rhs),
                                "andi" => input_bits.and(&rhs),
                                "xori" => input_bits.xor(&rhs),
                                "shli" => input_bits.shl(constant as u64),
                                "shrui" => input_bits.lshr(constant as u64),
                                "shrsi" => input_bits.ashr(constant as u64),
                                _ => unreachable!(),
                            }
                        }
                        .with_signed(false);
                        assert_eq!(execute(&context, function, width, value), expected);
                    }
                    assert!(matches!(
                        interp::run_function(&context, function, vec![Value::Poison])
                            .expect("poison executes")
                            .as_slice(),
                        [Value::Poison]
                    ));
                }
            }
        }
    }
}

#[test]
fn loop_body_rhs_keeps_feedback_and_other_scoped_rewrites() {
    let original = include_str!("../../../../core/checks/Inputs/eqsat/loop-body-rhs.tir");
    let mixed = original.replace("    %i1_end", "    %one = constant {value = 1} : !i32\n    %forward = muli %3, %one : !i32\n    %i1_end")
        .replace("(%6 = %3)", "(%6 = %forward)");
    let sibling = mixed.replace(
        "    func.return %5",
        r#"
    %i2_end, %last = scf.ordered_for %i2 = %0 to %1 step %2 (%v = %5) {
      %two2 = constant {value = 2} : !i32
      %next2 = muli %v, %two2 : !i32
      scf.yield %next2
    }
    func.return %last"#,
    );
    for (source, loops, flags, outer_alias) in [
        (original.to_owned(), 1, false, false),
        (mixed.clone(), 1, false, true),
        (sibling, 2, false, true),
        (
            mixed.replace("muli %6, %7 :", "muli %6, %7 {nsw = true} :"),
            1,
            true,
            true,
        ),
    ] {
        let (context, module, function, _, _) = parse(&source);
        let owners = context
            .get_region(context.get_op(function).regions()[0])
            .op_ids()
            .into_iter()
            .filter(|&op| !context.get_op(op).regions().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(owners.len(), loops);
        let structure = owners
            .iter()
            .map(|&owner| {
                let op = context.get_op(owner);
                let region = context.get_region(op.regions()[0]);
                (
                    op.results(),
                    op.attributes(),
                    region
                        .value_arguments()
                        .iter()
                        .map(|value| value.id())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let x = tir::func::func_parameters(
            &context
                .get_op(function)
                .as_operation(&tir::func::FUNC)
                .unwrap(),
        )[3]
        .id();
        let mut pass =
            EqSatPass::new().with_operation_cost(|kind| if kind == "builtin.muli" { 4 } else { 1 });
        for round in 0..2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            if round > 0 {
                for (&owner, (results, attributes, parameters)) in owners.iter().zip(&structure) {
                    let op = context.get_op(owner);
                    let region = context.get_region(op.regions()[0]);
                    assert_eq!(op.results(), *results);
                    assert_eq!(op.attributes(), *attributes);
                    assert_eq!(
                        region
                            .value_arguments()
                            .iter()
                            .map(|value| value.id())
                            .collect::<Vec<_>>(),
                        *parameters
                    );
                    let kinds = region
                        .op_ids()
                        .iter()
                        .map(|&op| context.get_op(op).name().as_str().to_owned())
                        .collect::<Vec<_>>();
                    assert_eq!(kinds.iter().any(|kind| kind == "shli"), !flags);
                    assert_eq!(kinds.iter().any(|kind| kind == "muli"), flags);
                }
                if outer_alias {
                    assert_eq!(
                        tir::scf::ordered_for_inits(
                            &context
                                .get_op(owners[0])
                                .as_operation(&tir::scf::ORDERED_FOR)
                                .unwrap()
                        )[0],
                        x
                    );
                }
            }
            for iterations in [0, 1, 3] {
                for input in [0, 7, -17, i32::MAX as i64] {
                    let args = vec![
                        Value::Int(APInt::new(64, 0)),
                        Value::Int(APInt::new(64, iterations)),
                        Value::Int(APInt::one(64)),
                        Value::Int(APInt::new_signed(32, input)),
                    ];
                    let actual = interp::run_function(&context, function, args).unwrap();
                    let [Value::Int(actual)] = actual.as_slice() else {
                        panic!("expected loop result")
                    };
                    assert_eq!(
                        actual.clone().with_signed(false),
                        APInt::new_signed(32, input)
                            .shl(iterations * loops as u64)
                            .with_signed(false)
                    );
                }
            }
        }
    }
}

#[test]
fn power_of_two_multiplication_obeys_width_guards_and_custom_costs() {
    for width in [1, 8, 16, 32, 64, 128] {
        for (literal, shift) in [("4", 2), ("8", 3), ("18446744073709551616", 64)] {
            let (context, module, function, _, _) = fixture(width, literal, "");
            let mut pass =
                EqSatPass::new()
                    .with_operation_cost(|kind| if kind == "builtin.muli" { 4 } else { 1 });
            for round in 0..2 {
                if round > 0 {
                    pass.run(function, &context, &AnalysisManager::new())
                        .unwrap();
                }
                tir::verify_op_tree(&context, module.id()).unwrap();
                let legal = shift < u64::from(width);
                if round > 0 {
                    let text = printed_ir(&context, &module);
                    assert_eq!(
                        text.contains("= shli "),
                        legal,
                        "i{width} * {literal}: {text}"
                    );
                    if legal {
                        assert!(!text.contains("= muli "), "{text}");
                    }
                }
                for value in [i64::MIN, -17, -1, 0, 1, i64::MAX] {
                    let expected = APInt::new_signed(width.into(), value)
                        .shl(shift)
                        .with_signed(false);
                    assert_eq!(execute(&context, function, width, value), expected);
                }
                assert!(matches!(
                    interp::run_function(&context, function, vec![Value::Poison])
                        .unwrap()
                        .as_slice(),
                    [Value::Poison]
                ));
            }
        }
        for constant in [0, 1, 3] {
            let (context, module, function, _, _) = fixture(width, constant, "");
            EqSatPass::new()
                .with_operation_cost(|kind| if kind == "builtin.muli" { 4 } else { 1 })
                .run(function, &context, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            assert!(!printed_ir(&context, &module).contains("= shli "));
            for value in [-7, 0, 1, 17] {
                let expected = APInt::new_signed(width.into(), value)
                    .mul(&APInt::new(width.into(), constant as u64))
                    .with_signed(false);
                assert_eq!(execute(&context, function, width, value), expected);
            }
        }
    }
    let (context, module, function, _, _) = parse(
        r#"module {
      %f = func.func @f(%x: !i32) -> !i32 {
        %one = constant {value = 1} : !i32
        %four = constant {value = 4} : !i32
        %fold = muli %one, %four : !i32
        %r = muli %x, %fold : !i32
        func.return %r
      }
      module_end
    }"#,
    );
    EqSatPass::new()
        .with_operation_cost(|kind| if kind == "builtin.muli" { 4 } else { 1 })
        .run(function, &context, &AnalysisManager::new())
        .unwrap();
    let text = printed_ir(&context, &module);
    assert!(
        !text.contains("= muli ") && text.contains("= shli "),
        "{text}"
    );
    tir::verify_op_tree(&context, module.id()).unwrap();
    assert_eq!(execute(&context, function, 32, 7), APInt::new(32, 28));
}

#[test]
fn a_custom_cost_selects_integer_negation_and_preserves_poison() {
    for width in [1, 8, 16, 32, 64, 128] {
        let (context, module, function, ret, input) = fixture(width, -1, "");
        let mut pass =
            EqSatPass::new().with_operation_cost(|kind| if kind == "builtin.muli" { 4 } else { 1 });
        for round in 0..2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            if round > 0 {
                let text = printed_ir(&context, &module);
                assert!(!text.contains("= muli "), "{text}");
                if width == 1 {
                    assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
                } else {
                    assert!(text.contains("= subi "), "{text}");
                }
            }
            for value in [i64::MIN, -17, -1, 0, 1, i64::MAX] {
                assert_eq!(
                    execute(&context, function, width, value),
                    APInt::zero(width.into())
                        .sub(&APInt::new_signed(width.into(), value))
                        .with_signed(false)
                );
            }
            assert!(matches!(
                interp::run_function(&context, function, vec![Value::Poison])
                    .unwrap()
                    .as_slice(),
                [Value::Poison]
            ));
        }
    }
}

#[test]
fn constant_offsets_reassociate_with_modular_overflow_and_poison() {
    use tir::sem::{ProofOutcome, SemGraph, SmtOracle, SymPayload};
    use tir_symbolic::{
        lang::SymKind,
        sem::{op, sym},
    };
    for (form, a, b, combined, kind) in [
        ("pair", 3, 5, 8_i128, SymKind::Add),
        ("mul", 3, 5, 15, SymKind::Mul),
        ("add", i64::MAX, 1, 9223372036854775808, SymKind::Add),
        ("sub", i64::MIN, 1, -9223372036854775809, SymKind::Add),
        ("mul", i64::MAX, 2, 18446744073709551614, SymKind::Mul),
        (
            "mul",
            4000000000,
            4000000000,
            16000000000000000000,
            SymKind::Mul,
        ),
    ] {
        for width in [32, 1, 8, 16, 64, 128] {
            let body = match form {
                "pair" => "%p = addi %x, %a : !iW\n%q = addi %y, %b : !iW\n%r = addi %p, %q : !iW",
                "add" => "%p = addi %x, %a : !iW\n%r = addi %p, %b : !iW",
                "sub" => "%p = addi %x, %a : !iW\n%r = subi %p, %b : !iW",
                "mul" => "%p = muli %x, %a : !iW\n%r = muli %p, %b : !iW",
                _ => unreachable!(),
            }
            .replace("!iW", &format!("!i{width}"));
            let source = format!(
                r#"module {{
              %f = func.func @f(%x: !i{width}, %y: !i{width}) -> !i{width} {{
                %a = constant {{value = {a}}} : !i{width}
                %b = constant {{value = {b}}} : !i{width}
                {body}
                func.return %r
              }}
              module_end
            }}"#
            );
            let coefficient =
                APInt::from_limbs(width.into(), &[combined as u64, (combined >> 64) as u64]);
            let (context, module, function, _, _) = parse(&source);
            let mut pass = EqSatPass::new();
            for round in 0..3 {
                if round > 0 {
                    pass.run(function, &context, &AnalysisManager::new())
                        .unwrap();
                }
                tir::verify_op_tree(&context, module.id()).unwrap();
                if round > 0 {
                    let text = printed_ir(&context, &module);
                    let count = ["= addi ", "= subi ", "= muli "]
                        .iter()
                        .map(|kind| text.matches(kind).count())
                        .sum::<usize>();
                    let expected = match form {
                        "pair" => 1 + usize::from(!coefficient.is_zero()),
                        "mul" => usize::from(coefficient != APInt::one(width.into())),
                        _ => usize::from(!coefficient.is_zero()),
                    };
                    assert_eq!(count, expected, "{form} {a} {b}, i{width}: {text}");
                }
                for (x, y) in [(10, 20), (-7, 11), (1, -1), (i64::MIN, i64::MAX)] {
                    let x = APInt::new_signed(width.into(), x);
                    let y = APInt::new_signed(width.into(), y);
                    let expected = match form {
                        "pair" => x.add(&y).add(&coefficient),
                        "mul" => x.mul(&coefficient),
                        _ => x.add(&coefficient),
                    }
                    .with_signed(false);
                    let actual = interp::run_function(
                        &context,
                        function,
                        vec![Value::Int(x), Value::Int(y)],
                    )
                    .unwrap();
                    let [Value::Int(actual)] = actual.as_slice() else {
                        panic!("expected integer")
                    };
                    assert_eq!(actual.clone().with_signed(false), expected);
                }
                assert!(matches!(
                    interp::run_function(
                        &context,
                        function,
                        vec![Value::Poison, Value::Int(APInt::zero(width.into()))]
                    )
                    .unwrap()
                    .as_slice(),
                    [Value::Poison]
                ));
                let actual = interp::run_function(
                    &context,
                    function,
                    vec![Value::Int(APInt::one(width.into())), Value::Poison],
                )
                .unwrap();
                if form == "pair" {
                    assert!(matches!(actual.as_slice(), [Value::Poison]));
                } else {
                    let [Value::Int(actual)] = actual.as_slice() else {
                        panic!("unused poison y must not affect x")
                    };
                    let expected = if form == "mul" {
                        coefficient.clone()
                    } else {
                        APInt::one(width.into()).add(&coefficient)
                    }
                    .with_signed(false);
                    assert_eq!(actual.clone().with_signed(false), expected);
                }
            }
            if form == "mul" && width > 8 {
                // Wide multiplication uses the modular associativity law; check its folded coefficient independently.
                let mut product = SemGraph::new();
                let [left, right] = [a, b].map(|value| {
                    let node = product.add_node(SymKind::Constant);
                    product.set_leaf_data(
                        node,
                        SymPayload::Int(APInt::new_signed(width.into(), value)),
                    );
                    node
                });
                op(&mut product, SymKind::Mul, &[left, right]);
                let mut expected = SemGraph::new();
                let node = expected.add_node(SymKind::Constant);
                expected.set_leaf_data(node, SymPayload::Int(coefficient));
                assert_eq!(
                    SmtOracle.equivalent_outcome(&product, &expected, &[]),
                    ProofOutcome::Proven
                );
                continue;
            }
            let mut lhs = SemGraph::new();
            let x = sym(&mut lhs, 0);
            let y = sym(&mut lhs, 1);
            let [a_node, b_node] = [a, b].map(|value| {
                let node = lhs.add_node(SymKind::Constant);
                lhs.set_leaf_data(
                    node,
                    SymPayload::Int(APInt::new_signed(width.into(), value)),
                );
                node
            });
            let first = op(&mut lhs, kind, &[x, a_node]);
            let last = if form == "pair" {
                op(&mut lhs, SymKind::Add, &[y, b_node])
            } else {
                b_node
            };
            op(
                &mut lhs,
                if form == "sub" { SymKind::Sub } else { kind },
                &[first, last],
            );
            let mut rhs = SemGraph::new();
            let x = sym(&mut rhs, 0);
            let y = sym(&mut rhs, 1);
            let c = rhs.add_node(SymKind::Constant);
            rhs.set_leaf_data(c, SymPayload::Int(coefficient));
            let base = if form == "pair" {
                op(&mut rhs, SymKind::Add, &[x, y])
            } else {
                x
            };
            op(&mut rhs, kind, &[base, c]);
            assert_eq!(
                SmtOracle.equivalent_outcome(&lhs, &rhs, &[width; 2]),
                ProofOutcome::Proven,
                "{form}, i{width}"
            );
        }
    }
}

#[test]
fn computed_constants_saturate_and_publish_modular_integer_results() {
    for width in [32, 1, 8, 16, 64, 128, 129, 200] {
        for (operator, a, b, expected) in [
            ("addi", 2, 3, 5),
            ("addi", 3, 4, 7),
            ("subi", 0, 1, -1),
            ("muli", 128, 2, 256),
        ] {
            let source = format!(
                r#"module {{
              %f = func.func @f() -> !i{width} {{
                %a = constant {{value = {a}}} : !i{width}
                %b = constant {{value = {b}}} : !i{width}
                %r = {operator} %a, %b : !i{width}
                func.return %r
              }}
              module_end
            }}"#
            );
            let context = Context::with_default_dialects();
            let module = parse_operation(&context, &source, &MODULE).unwrap();
            let function = context
                .get_region(context.get_op(module.id()).regions()[0])
                .op_ids()[0];
            let mut pass = EqSatPass::new();
            for round in 0..3 {
                if round > 0 {
                    pass.run(function, &context, &AnalysisManager::new())
                        .unwrap();
                }
                tir::verify_op_tree(&context, module.id()).unwrap();
                let actual = interp::run_function(&context, function, vec![]).unwrap();
                let [Value::Int(actual)] = actual.as_slice() else {
                    panic!("expected integer")
                };
                assert_eq!(
                    actual.clone().with_signed(false),
                    APInt::new_signed(width, expected).with_signed(false)
                );
                if round > 0 {
                    let text = printed_ir(&context, &module);
                    assert!(!text.contains(&format!("= {operator} ")), "{text}");
                    assert_eq!(text.matches("= constant ").count(), 1, "{text}");
                }
            }
        }
    }
    let source = r#"module {
      %f = func.func @f() -> !i32 {
        %a = constant {value = 2} : !i32
        %b = constant {value = 3} : !i32
        %c = constant {value = 4} : !i32
        %sum = addi %a, %b : !i32
        %r = muli %sum, %c : !i32
        func.return %r
      }
      module_end
    }"#;
    let context = Context::with_default_dialects();
    let module = parse_operation(&context, source, &MODULE).unwrap();
    let function = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()[0];
    EqSatPass::new()
        .run(function, &context, &AnalysisManager::new())
        .unwrap();
    tir::verify_op_tree(&context, module.id()).unwrap();
    let text = printed_ir(&context, &module);
    assert!(
        !text.contains("= addi ") && !text.contains("= muli "),
        "{text}"
    );
    let actual = interp::run_function(&context, function, vec![]).unwrap();
    assert_eq!(actual[0].to_i64(), Some(20));
}

#[test]
fn affine_cancellation_keeps_unknown_poison_dependencies() {
    use tir::sem::{ProofOutcome, SemGraph, SmtOracle};
    use tir_symbolic::{
        lang::SymKind,
        sem::{con, op, sym},
    };
    for (case, body, guard, target, original_ops) in [
        (
            "telescope",
            "%a = subi %x, %y : !iW\n%b = subi %y, %z : !iW\n%r = addi %a, %b : !iW",
            "%y",
            "x-z",
            3,
        ),
        (
            "add-add-sub",
            "%a = addi %x, %y : !iW\n%b = subi %z, %y : !iW\n%r = addi %a, %b : !iW",
            "%y",
            "x+z",
            3,
        ),
        (
            "sub-add-left",
            "%a = addi %x, %y : !iW\n%b = addi %x, %z : !iW\n%r = subi %a, %b : !iW",
            "%x",
            "y-z",
            3,
        ),
        (
            "sub-add-negate",
            "%a = addi %x, %y : !iW\n%r = subi %x, %a : !iW",
            "%x",
            "0-y",
            2,
        ),
        (
            "sub-left",
            "%a = subi %x, %y : !iW\n%b = subi %x, %z : !iW\n%r = subi %a, %b : !iW",
            "%x",
            "z-y",
            3,
        ),
        (
            "sub-right",
            "%a = subi %x, %y : !iW\n%b = subi %z, %y : !iW\n%r = subi %a, %b : !iW",
            "%y",
            "x-z",
            3,
        ),
        (
            "sub-sub-left",
            "%a = subi %x, %y : !iW\n%r = subi %a, %x : !iW",
            "%x",
            "0-y",
            2,
        ),
        (
            "sub-sub-cancel",
            "%a = subi %y, %x : !iW\n%r = subi %y, %a : !iW",
            "%y",
            "x",
            2,
        ),
    ] {
        for width in [1, 8, 16, 32, 64, 128] {
            for defined in [false, true] {
                let body = if defined {
                    body.replace(guard, "%known")
                } else {
                    body.to_owned()
                };
                let active = ["%x", "%y", "%z"].map(|name| body.contains(name));
                let body = body.replace("!iW", &format!("!i{width}"));
                let source = format!(
                    r#"module {{
                  %f = func.func @f(%x: !i{width}, %y: !i{width}, %z: !i{width}) -> !i{width} {{
                    %known = constant {{value = 7}} : !i{width}
                    {body}
                    func.return %r
                  }}
                  module_end
                }}"#
                );
                let (context, module, function, _, _) = parse(&source);
                let mut pass = EqSatPass::new();
                for round in 0..2 {
                    if round > 0 {
                        pass.run(function, &context, &AnalysisManager::new())
                            .unwrap();
                    }
                    tir::verify_op_tree(&context, module.id()).unwrap();
                    let text = printed_ir(&context, &module);
                    let count = text.matches("= addi ").count() + text.matches("= subi ").count();
                    assert_eq!(
                        count,
                        if round > 0 && defined {
                            usize::from(target != "x")
                        } else {
                            original_ops
                        },
                        "{case}, defined={defined}, i{width}: {text}"
                    );
                    for values in [[-7, 11, 19], [i64::MIN, i64::MAX, 0], [0, 0, 0], [1, -1, 2]] {
                        let [x, y, z] = values.map(|value| APInt::new_signed(width.into(), value));
                        let expected = match target {
                            "x-z" => x.sub(&z),
                            "x+z" => x.add(&z),
                            "y-z" => y.sub(&z),
                            "0-y" => APInt::zero(width.into()).sub(&y),
                            "z-y" => z.sub(&y),
                            "x" => x.clone(),
                            _ => unreachable!(),
                        }
                        .with_signed(false);
                        for poison in [None, Some(0), Some(1), Some(2)] {
                            let mut args = vec![
                                Value::Int(x.clone()),
                                Value::Int(y.clone()),
                                Value::Int(z.clone()),
                            ];
                            if let Some(index) = poison {
                                args[index] = Value::Poison;
                            }
                            let actual = interp::run_function(&context, function, args).unwrap();
                            if poison.is_some_and(|index| active[index]) {
                                assert!(
                                    matches!(actual.as_slice(), [Value::Poison]),
                                    "{case}, {defined}, {poison:?}"
                                );
                            } else {
                                let [Value::Int(actual)] = actual.as_slice() else {
                                    panic!("expected integer result")
                                };
                                assert_eq!(
                                    actual.clone().with_signed(false),
                                    expected,
                                    "{case}, {defined}, {poison:?}"
                                );
                            }
                        }
                    }
                }
            }
            let mut lhs = SemGraph::new();
            let x = sym(&mut lhs, 0);
            let y = sym(&mut lhs, 1);
            let z = sym(&mut lhs, 2);
            let xy_sub = op(&mut lhs, SymKind::Sub, &[x, y]);
            let yz_sub = op(&mut lhs, SymKind::Sub, &[y, z]);
            let xy_add = op(&mut lhs, SymKind::Add, &[x, y]);
            let zy_sub = op(&mut lhs, SymKind::Sub, &[z, y]);
            let xz_add = op(&mut lhs, SymKind::Add, &[x, z]);
            let xz_sub = op(&mut lhs, SymKind::Sub, &[x, z]);
            let yx_sub = op(&mut lhs, SymKind::Sub, &[y, x]);
            match case {
                "telescope" => op(&mut lhs, SymKind::Add, &[xy_sub, yz_sub]),
                "add-add-sub" => op(&mut lhs, SymKind::Add, &[xy_add, zy_sub]),
                "sub-add-left" => op(&mut lhs, SymKind::Sub, &[xy_add, xz_add]),
                "sub-add-negate" => op(&mut lhs, SymKind::Sub, &[x, xy_add]),
                "sub-left" => op(&mut lhs, SymKind::Sub, &[xy_sub, xz_sub]),
                "sub-right" => op(&mut lhs, SymKind::Sub, &[xy_sub, zy_sub]),
                "sub-sub-left" => op(&mut lhs, SymKind::Sub, &[xy_sub, x]),
                "sub-sub-cancel" => op(&mut lhs, SymKind::Sub, &[y, yx_sub]),
                _ => unreachable!(),
            };
            let mut rhs = SemGraph::new();
            let x = sym(&mut rhs, 0);
            let y = sym(&mut rhs, 1);
            let z = sym(&mut rhs, 2);
            let zero = con(&mut rhs, 0, width);
            match target {
                "x-z" => op(&mut rhs, SymKind::Sub, &[x, z]),
                "x+z" => op(&mut rhs, SymKind::Add, &[x, z]),
                "y-z" => op(&mut rhs, SymKind::Sub, &[y, z]),
                "0-y" => op(&mut rhs, SymKind::Sub, &[zero, y]),
                "z-y" => op(&mut rhs, SymKind::Sub, &[z, y]),
                "x" => sym(&mut rhs, 0),
                _ => unreachable!(),
            };
            assert_eq!(
                SmtOracle.equivalent_outcome(&lhs, &rhs, &[width; 3]),
                ProofOutcome::Proven,
                "{case}, i{width}"
            );
        }
    }
}

#[test]
fn absorption_and_cancellation_require_the_discarded_operand_to_be_defined() {
    use tir::sem::{ProofOutcome, SemGraph, SmtOracle};
    use tir_symbolic::{
        lang::SymKind,
        sem::{op, sym},
    };
    for (first, outer, first_kind, outer_kind, cancel) in [
        ("ori", "andi", SymKind::Or, SymKind::And, false),
        ("andi", "ori", SymKind::And, SymKind::Or, false),
        ("xori", "xori", SymKind::Xor, SymKind::Xor, true),
        ("subi", "addi", SymKind::Sub, SymKind::Add, true),
        ("addi", "subi", SymKind::Add, SymKind::Sub, true),
    ] {
        for width in [1, 8, 16, 32, 64, 128] {
            for defined in [false, true] {
                let y = if defined { "%known" } else { "%y" };
                let last = if cancel { y } else { "%x" };
                let source = format!(
                    r#"module {{
                  %f = func.func @f(%x: !i{width}, %y: !i{width}) -> !i{width} {{
                    %known = constant {{value = 7}} : !i{width}
                    %a = {first} %x, {y} : !i{width}
                    %r = {outer} %a, {last} : !i{width}
                    func.return %r
                  }}
                  module_end
                }}"#
                );
                let (context, module, function, ret, input) = parse(&source);
                let mut pass = EqSatPass::new();
                for round in 0..2 {
                    if round > 0 {
                        pass.run(function, &context, &AnalysisManager::new())
                            .unwrap();
                    }
                    tir::verify_op_tree(&context, module.id()).unwrap();
                    let text = printed_ir(&context, &module);
                    let count = ["= andi ", "= ori ", "= xori ", "= addi ", "= subi "]
                        .iter()
                        .map(|kind| text.matches(kind).count())
                        .sum::<usize>();
                    assert_eq!(
                        count,
                        if round > 0 && defined { 0 } else { 2 },
                        "{first}/{outer}, defined={defined}, i{width}: {text}"
                    );
                    if round > 0 && defined {
                        assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
                    }
                    for (x, y) in [(0, 0), (-1, 7), (17, -35), (i64::MIN, i64::MAX)] {
                        let x = APInt::new_signed(width.into(), x);
                        let y = APInt::new_signed(width.into(), y);
                        let actual = interp::run_function(
                            &context,
                            function,
                            vec![Value::Int(x.clone()), Value::Int(y)],
                        )
                        .unwrap();
                        let [Value::Int(actual)] = actual.as_slice() else {
                            panic!("expected integer")
                        };
                        assert_eq!(actual.clone().with_signed(false), x.with_signed(false));
                    }
                    for (poison_x, poison_y) in [(true, false), (false, true), (true, true)] {
                        let value = Value::Int(APInt::new(width.into(), 7));
                        let args = vec![
                            if poison_x {
                                Value::Poison
                            } else {
                                value.clone()
                            },
                            if poison_y { Value::Poison } else { value },
                        ];
                        let actual = interp::run_function(&context, function, args).unwrap();
                        if poison_x || poison_y && !defined {
                            assert!(matches!(actual.as_slice(), [Value::Poison]));
                        } else {
                            let [Value::Int(actual)] = actual.as_slice() else {
                                panic!("unused poison must not affect x")
                            };
                            assert_eq!(
                                actual.clone().with_signed(false),
                                APInt::new(width.into(), 7)
                            );
                        }
                    }
                }
            }
            let mut lhs = SemGraph::new();
            let x = sym(&mut lhs, 0);
            let y = sym(&mut lhs, 1);
            let first = op(&mut lhs, first_kind, &[x, y]);
            op(&mut lhs, outer_kind, &[first, if cancel { y } else { x }]);
            let mut rhs = SemGraph::new();
            sym(&mut rhs, 0);
            assert_eq!(
                SmtOracle.equivalent_outcome(&lhs, &rhs, &[width; 2]),
                ProofOutcome::Proven
            );
        }
    }
}

#[test]
fn constant_results_require_defined_inputs_and_preserve_poison() {
    use tir::sem::{ProofOutcome, SemGraph, SmtOracle};
    use tir_symbolic::{
        lang::SymKind,
        sem::{con, op, sym},
    };
    for (case, body, result, original_ops) in [
        (
            "and-zero",
            "%mask = constant {value = 0} : !iW\n%r = andi ARG, %mask : !iW",
            0,
            1,
        ),
        (
            "or-ones",
            "%mask = constant {value = -1} : !iW\n%r = ori ARG, %mask : !iW",
            -1,
            1,
        ),
        ("xor-self", "%r = xori ARG, ARG : !iW", 0, 1),
        ("sub-self", "%r = subi ARG, ARG : !iW", 0, 1),
        (
            "and-complement",
            "%mask = constant {value = -1} : !iW\n%n = xori ARG, %mask : !iW\n%r = andi ARG, %n : !iW",
            0,
            2,
        ),
        (
            "or-complement",
            "%mask = constant {value = -1} : !iW\n%n = xori ARG, %mask : !iW\n%r = ori ARG, %n : !iW",
            -1,
            2,
        ),
    ] {
        for width in [1, 8, 16, 32, 64, 128] {
            for defined in [false, true] {
                let body = body
                    .replace("ARG", if defined { "%known" } else { "%x" })
                    .replace("!iW", &format!("!i{width}"));
                let source = format!(
                    r#"module {{
                  %f = func.func @f(%x: !i{width}) -> !i{width} {{
                    %known = constant {{value = 7}} : !i{width}
                    {body}
                    func.return %r
                  }}
                  module_end
                }}"#
                );
                let (context, module, function, _, _) = parse(&source);
                let mut pass = EqSatPass::new();
                for round in 0..2 {
                    if round > 0 {
                        pass.run(function, &context, &AnalysisManager::new())
                            .unwrap();
                    }
                    tir::verify_op_tree(&context, module.id()).unwrap();
                    let text = printed_ir(&context, &module);
                    let operations = ["= andi ", "= ori ", "= xori ", "= subi "]
                        .iter()
                        .map(|kind| text.matches(kind).count())
                        .sum::<usize>();
                    assert_eq!(
                        operations,
                        if round > 0 && defined {
                            0
                        } else {
                            original_ops
                        },
                        "{case}, defined={defined}, i{width}: {text}"
                    );
                    let expected = APInt::new_signed(width.into(), result).with_signed(false);
                    for input in [i64::MIN, -1, 0, 17, i64::MAX] {
                        assert_eq!(execute(&context, function, width, input), expected);
                    }
                    let actual =
                        interp::run_function(&context, function, vec![Value::Poison]).unwrap();
                    if defined {
                        let [Value::Int(actual)] = actual.as_slice() else {
                            panic!("unused poison input must not affect a constant")
                        };
                        assert_eq!(actual.clone().with_signed(false), expected);
                    } else {
                        assert!(matches!(actual.as_slice(), [Value::Poison]));
                    }
                }
            }
            let mut lhs = SemGraph::new();
            let x = sym(&mut lhs, 0);
            let zero = con(&mut lhs, 0, width);
            let ones = op(&mut lhs, SymKind::Not, &[zero]);
            let complement = op(&mut lhs, SymKind::Xor, &[x, ones]);
            match case {
                "and-zero" => op(&mut lhs, SymKind::And, &[x, zero]),
                "or-ones" => op(&mut lhs, SymKind::Or, &[x, ones]),
                "xor-self" => op(&mut lhs, SymKind::Xor, &[x, x]),
                "sub-self" => op(&mut lhs, SymKind::Sub, &[x, x]),
                "and-complement" => op(&mut lhs, SymKind::And, &[x, complement]),
                "or-complement" => op(&mut lhs, SymKind::Or, &[x, complement]),
                _ => unreachable!(),
            };
            let mut rhs = SemGraph::new();
            let zero = con(&mut rhs, 0, width);
            if result == -1 {
                op(&mut rhs, SymKind::Not, &[zero]);
            }
            assert_eq!(
                SmtOracle.equivalent_outcome(&lhs, &rhs, &[width]),
                ProofOutcome::Proven,
                "{case} i{width}"
            );
        }
    }
}

#[test]
fn default_bitwise_compositions_reduce_operations_and_preserve_poison() {
    use tir::sem::{ProofOutcome, SemGraph, SmtOracle};
    use tir_symbolic::{
        lang::SymKind,
        sem::{op, sym},
    };
    for (first, target, first_kind, target_kind) in [
        ("ori", "xori", SymKind::Or, SymKind::Xor),
        ("xori", "ori", SymKind::Xor, SymKind::Or),
    ] {
        for width in [1, 8, 16, 32, 64, 128] {
            let source = format!(
                r#"module {{
              %f = func.func @f(%x: !i{width}, %y: !i{width}) -> !i{width} {{
                %a = {first} %x, %y : !i{width}
                %b = andi %x, %y : !i{width}
                %r = xori %a, %b : !i{width}
                func.return %r
              }}
              module_end
            }}"#
            );
            let (context, module, function, _, _) = parse(&source);
            let mut pass = EqSatPass::new();
            for round in 0..2 {
                if round > 0 {
                    pass.run(function, &context, &AnalysisManager::new())
                        .unwrap();
                }
                tir::verify_op_tree(&context, module.id()).unwrap();
                let text = printed_ir(&context, &module);
                let count = ["= ori ", "= andi ", "= xori "]
                    .iter()
                    .map(|kind| text.matches(kind).count())
                    .sum::<usize>();
                assert_eq!(
                    count,
                    if round == 0 { 3 } else { 1 },
                    "{first}, i{width}: {text}"
                );
                if round > 0 {
                    assert!(text.contains(&format!("= {target} ")), "{text}");
                }
                for (x, y) in [(0, 0), (-1, 3), (17, -35), (i64::MIN, i64::MAX)] {
                    let x = APInt::new_signed(width.into(), x);
                    let y = APInt::new_signed(width.into(), y);
                    let expected = if target == "xori" {
                        x.xor(&y)
                    } else {
                        x.or(&y)
                    }
                    .with_signed(false);
                    let actual = interp::run_function(
                        &context,
                        function,
                        vec![Value::Int(x), Value::Int(y)],
                    )
                    .unwrap();
                    let [Value::Int(actual)] = actual.as_slice() else {
                        panic!("expected integer result")
                    };
                    assert_eq!(actual.clone().with_signed(false), expected);
                }
                for args in [
                    vec![Value::Poison, Value::Int(APInt::new(width.into(), 7))],
                    vec![Value::Int(APInt::new(width.into(), 7)), Value::Poison],
                    vec![Value::Poison; 2],
                ] {
                    assert!(matches!(
                        interp::run_function(&context, function, args)
                            .unwrap()
                            .as_slice(),
                        [Value::Poison]
                    ));
                }
            }
            let mut lhs = SemGraph::new();
            let x = sym(&mut lhs, 0);
            let y = sym(&mut lhs, 1);
            let a = op(&mut lhs, first_kind, &[x, y]);
            let b = op(&mut lhs, SymKind::And, &[x, y]);
            op(&mut lhs, SymKind::Xor, &[a, b]);
            let mut rhs = SemGraph::new();
            let x = sym(&mut rhs, 0);
            let y = sym(&mut rhs, 1);
            op(&mut rhs, target_kind, &[x, y]);
            assert_eq!(
                SmtOracle.equivalent_outcome(&lhs, &rhs, &[width; 2]),
                ProofOutcome::Proven
            );
        }
    }
}

#[test]
fn shared_bitwise_results_preserve_live_stores_and_canonical_operands() {
    for overwritten in [false, true] {
        let source = r#"module {
          %f = func.func @xor_xor_and_reversed(%x: !i32, %y: !i32, %p: !ptr.p<!i32>) -> !i32 {
            %xor = xori %y, %x : !i32
            %and = andi %y, %x : !i32
            %result = xori %and, %xor : !i32
            ptr.store %xor, %p
            func.return %result
          }
          module_end
        }"#;
        let source = if overwritten {
            source.replace(
                "ptr.store %xor, %p",
                "ptr.store %xor, %p\n            ptr.store %result, %p",
            )
        } else {
            source.to_owned()
        };
        let (context, module, function, _, _) = parse(&source);
        let mut pass = EqSatPass::new();
        for round in 0..2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                let engine = context.egraph_with_rules().unwrap();
                let db = engine.database();
                let kinds = db.table("OpKind").unwrap();
                assert!(
                    db.rows(db.table("Operand").unwrap())
                        .all(|row| db.lookup(kinds, &[row[0]]).is_some()),
                    "every Operand row must belong to a live operation"
                );
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            let text = printed_ir(&context, &module);
            assert_eq!(
                text.matches("= xori ").count(),
                if round == 0 {
                    2
                } else {
                    usize::from(!overwritten)
                },
                "{text}"
            );
            assert_eq!(
                text.matches("= andi ").count(),
                usize::from(round == 0),
                "{text}"
            );
            assert_eq!(
                text.matches("= ori ").count(),
                usize::from(round > 0),
                "{text}"
            );
            assert_eq!(
                text.matches("ptr.store ").count(),
                if round == 0 && overwritten { 2 } else { 1 },
                "{text}"
            );
            for (x, y) in [(0_i32, 0_i32), (-1, 3), (17, -35), (i32::MIN, i32::MAX)] {
                for poison in [None, Some(0), Some(1), Some(2)] {
                    for fault in [false, true] {
                        assert_shared_store(&context, function, overwritten, (x, y, poison, fault));
                    }
                }
            }
        }
    }
}

fn assert_shared_store(
    context: &Context,
    function: tir::OpId,
    overwritten: bool,
    (x, y, poison, fault): (i32, i32, Option<usize>, bool),
) {
    let mut state = interp::ExecutionState::default();
    let base = state.memory.alloc(8, 1);
    state.memory.write(base, &[0xa5; 8]).unwrap();
    let mut args = vec![
        Value::Int(APInt::new_signed(32, i64::from(x))),
        Value::Int(APInt::new_signed(32, i64::from(y))),
        Value::Ptr(base + if fault { 7 } else { 2 }),
    ];
    if let Some(index) = poison {
        args[index] = Value::Poison;
    }
    let result = interp::run_function_in_state(context, function, args, &mut state, Some(64));
    if poison == Some(2) {
        assert!(matches!(result, Err(interp::InterpError::Undefined(_))));
        assert_eq!(state.memory.read(base, 8).unwrap(), [0xa5; 8]);
    } else if fault {
        assert!(matches!(
            result,
            Err(interp::InterpError::OutOfBounds { size: 4, .. })
        ));
        assert_eq!(state.memory.read(base, 8).unwrap(), [0xa5; 8]);
    } else {
        assert_eq!(state.memory.read(base, 2).unwrap(), [0xa5; 2]);
        assert_eq!(state.memory.read(base + 6, 2).unwrap(), [0xa5; 2]);
        if poison.is_some() {
            assert!(matches!(result.unwrap().as_slice(), [Value::Poison]));
            assert!(matches!(
                state.memory.read(base + 2, 4),
                Err(interp::InterpError::Uninitialized { size: 4, .. })
            ));
        } else {
            let values = result.unwrap();
            let [Value::Int(actual)] = values.as_slice() else {
                panic!("expected integer result")
            };
            assert_eq!(
                actual.with_signed(false),
                APInt::new(32, u64::from((x | y) as u32))
            );
            assert_eq!(
                state.memory.read(base + 2, 4).unwrap(),
                (if overwritten { x | y } else { x ^ y }).to_le_bytes()
            );
        }
    }
}

#[test]
fn default_add_sub_zero_publishes_an_unordered_graph_input_without_arithmetic() {
    for operator in ["addi", "subi"] {
        use tir::builtin;
        let context = Context::with_default_dialects();
        let source = format!(
            r#"module {{
  %f = func.func @graph(%x: !i32) -> !i32 {{
    %result = {operator} %x, %zero : !i32
    %zero = constant {{value = 0}} : !i32
    -> %result
  }}
  module_end
}}"#
        );
        let module = parse_operation(&context, &source, &MODULE).unwrap();
        let function = context.get_op(context.get_region(module.regions()[0]).op_ids()[0]);
        let body = context.get_region(function.regions()[0]);
        let mut pass = EqSatPass::new();
        for round in 0..3 {
            if round > 0 {
                pass.run(function.id(), &context, &AnalysisManager::new())
                    .expect("graph pass runs");
                assert_eq!(body.results(), [body.ports()[0].id()]);
                eprintln!(
                    "{operator}-zero graph round={round}, published={:?}",
                    body.results()
                );
                assert!(!body.op_ids().iter().any(|&id| if operator == "addi" {
                    context.get_op(id).is_operation(&builtin::ADDI)
                } else {
                    context.get_op(id).is_operation(&builtin::SUBI)
                }));
            }
            tir::verify_op_tree(&context, function.id()).expect("graph verifies");
            for value in [-17, 0, i32::MAX as i64] {
                assert_eq!(
                    execute(&context, function.id(), 32, value),
                    APInt::new_signed(32, value).with_signed(false)
                );
            }
            assert!(matches!(
                interp::run_function(&context, function.id(), vec![Value::Poison])
                    .unwrap()
                    .as_slice(),
                [Value::Poison]
            ));
        }
    }
}

#[test]
fn add_sub_zero_checks_declared_value_poison_and_requires_at_checked_widths() {
    for operator in ["addi", "subi"] {
        use tir::sem::{ProofOutcome, SemGraph, SmtOracle, SymPayload};
        use tir_symbolic::sem::{con, copy_subgraph_with, sym, CopyAction};

        for width in [1, 8, 16, 32, 64, 128] {
            let (context, _, function, _, _) = identity_fixture(operator, width, 0, "");
            let body = context.get_region(context.get_op(function).regions()[0]);
            let block = body.iter(context.clone()).next().expect("entry block");
            let operation = context.get_op(block.op_ids()[1]).as_dyn_op();
            assert!(operation.requires_expr(&mut SemGraph::new()).is_none());
            for poison in [false, true] {
                let mut declared = SemGraph::new();
                let root = if poison {
                    operation
                        .poison_expr(&mut declared)
                        .expect("declared poison predicate")
                } else {
                    operation
                        .semantic_expr(&mut declared)
                        .expect("declared value semantics")
                };
                let mut lhs = SemGraph::new();
                copy_subgraph_with(
                    &mut lhs,
                    &declared,
                    root,
                    &mut std::collections::HashMap::new(),
                    &mut |graph, node| {
                        if matches!(declared.get_leaf_data(node), Some(SymPayload::SymbolId(1))) {
                            CopyAction::Replace(con(graph, 0, width))
                        } else {
                            CopyAction::Keep
                        }
                    },
                );
                let mut rhs = SemGraph::new();
                if poison {
                    con(&mut rhs, 0, 1);
                } else {
                    sym(&mut rhs, 0);
                }
                assert_eq!(
                    SmtOracle.equivalent_outcome(&lhs, &rhs, &[width]),
                    ProofOutcome::Proven,
                    "{operator}, i{width}, poison predicate={poison}"
                );
                eprintln!(
                    "{operator} i{width}, poison predicate={poison}: proven; requires absent"
                );
            }
        }
    }
}

#[test]
fn saturation_consumes_equalities_from_earlier_matches() {
    let (context, module, function, ret, input) = parse(
        r#"module {
  %f = func.func @f(%x: !i32) -> !i32 {
    %one = constant {value = 1} : !i32
    %identity = muli %one, %one : !i32
    %y = muli %x, %identity : !i32
    %z = muli %y, %identity : !i32
    func.return %z
  }
  module_end
}"#,
    );
    let before = execute(&context, function, 32, -17);
    EqSatPass::new()
        .run(function, &context, &AnalysisManager::new())
        .expect("eqsat runs");
    assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
    assert_eq!(execute(&context, function, 32, -17), before);
    tir::verify_op_tree(&context, module.id()).expect("published IR verifies");
}

#[test]
fn duplicate_expressions_reuse_only_an_available_origin() {
    let context = Context::with_default_dialects();
    let module = parse_operation(
        &context,
        r#"module {
  %f = func.func @f(%x: !i32, %y: !i32, %condition: !i1) -> !i32 {
    cfg.cond_br %condition, ^left, ^right
  ^left:
    %a = muli %x, %y : !i32
    %one = constant {value = 1} : !i32
    %b = muli %a, %one : !i32
    func.return %b
  ^right:
    %c = muli %x, %y : !i32
    %another = constant {value = 1} : !i32
    %d = muli %c, %another : !i32
    func.return %d
  }
  module_end
}"#,
        &MODULE,
    )
    .expect("input parses");
    let function = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()[0];
    tir::verify_op_tree(&context, module.id()).expect("input verifies");
    let body = context.get_region(context.get_op(function).regions()[0]);
    let cases: Vec<_> = body
        .iter(context.clone())
        .skip(1)
        .map(|block| {
            let operations = block.op_ids();
            (
                *operations.last().expect("return"),
                context.get_op(operations[0]).results()[0],
            )
        })
        .collect();
    EqSatPass::new()
        .run(function, &context, &AnalysisManager::new())
        .expect("eqsat runs");
    for (ret, expected) in cases {
        assert_eq!(context.get_op(ret).operands().as_slice(), [expected]);
    }
    tir::verify_op_tree(&context, module.id()).expect("origins dominate their uses");
    for condition in [0, 1] {
        let result = interp::run_function(
            &context,
            function,
            vec![
                Value::Int(APInt::new_signed(32, -7)),
                Value::Int(APInt::new_signed(32, 3)),
                Value::Int(APInt::new(1, condition)),
            ],
        )
        .expect("each arm executes");
        assert_eq!(result[0].to_i64(), Some(-21));
    }
}

#[test]
fn integer_literals_match_their_value_at_the_result_width() {
    for (width, constant) in [
        (1, "-1"),
        (8, "257"),
        (8, "-255"),
        (64, "18446744073709551617"),
        (128, "340282366920938463463374607431768211457"),
    ] {
        let (context, module, function, ret, input) = fixture(width, constant, "");
        let before = execute(&context, function, width, 7);
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .expect("semantic constant one matches");
        assert_eq!(
            context.get_op(ret).operands().as_slice(),
            [input],
            "i{width} {constant}"
        );
        assert_eq!(execute(&context, function, width, 7), before);
        tir::verify_op_tree(&context, module.id()).expect("published IR verifies");
    }
}

#[test]
fn graph_region_yields_publish_through_the_same_pass() {
    let context = Context::with_default_dialects();
    let module = parse_operation(
        &context,
        r#"module {
  %f = func.func @graph(%x: !i128) -> !i128 {
    %result = muli %x, %one : !i128
    %one = constant {value = 1} : !i128
    -> %result
  }
  module_end
}"#,
        &MODULE,
    )
    .unwrap();
    let function = context.get_op(context.get_region(module.regions()[0]).op_ids()[0]);
    let body = context.get_region(function.regions()[0]);
    tir::verify_op_tree(&context, function.id()).expect("unordered input verifies");
    let before = execute(&context, function.id(), 128, -19);
    EqSatPass::new()
        .run(function.id(), &context, &AnalysisManager::new())
        .expect("same pass accepts unordered input");
    assert_eq!(body.results(), [body.ports()[0].id()]);
    assert_eq!(execute(&context, function.id(), 128, -19), before);
    tir::verify_op_tree(&context, function.id()).expect("published graph verifies");
}

#[test]
fn type_incompatible_trusted_patterns_cannot_merge_values() {
    let library = tir_pdl::compile("rule wrong: builtin.trunci(x: int<64>) => x proof trusted;")
        .expect("valid PDL");
    let mut pass = EqSatPass::with_library(library).expect("supported pattern syntax");
    let (context, module, function, ret, _) = parse(
        r#"module {
  %f = func.func @f(%x: !i64) -> !i32 {
    %narrow = trunci %x : !i32
    func.return %narrow
  }
  module_end
}"#,
    );
    let before = context.get_op(ret).operands();
    pass.run(function, &context, &AnalysisManager::new())
        .expect("mismatched types do not union");
    assert_eq!(context.get_op(ret).operands(), before);
    tir::verify_op_tree(&context, module.id()).expect("types remain valid");
    let (context, module, function, _, _) = parse(include_str!(
        "../../../../core/checks/EqSat/type_in_key.tir"
    ));
    let before = printed_ir(&context, &module);
    EqSatPass::new()
        .run(function, &context, &AnalysisManager::new())
        .unwrap();
    assert_eq!(printed_ir(&context, &module), before);
    tir::verify_op_tree(&context, module.id()).unwrap();
    for input in [0, 127, 128, 255, 256, 0x123480ff, u32::MAX as i64] {
        let expected = (input as u16).wrapping_add((input as u8 as i8 as i16) as u16);
        assert_eq!(
            execute(&context, function, 32, input),
            APInt::new(16, u64::from(expected))
        );
    }
    assert!(matches!(
        interp::run_function(&context, function, vec![Value::Poison])
            .unwrap()
            .as_slice(),
        [Value::Poison]
    ));
}

#[test]
fn a_rewrite_budget_retains_executable_original_witnesses() {
    let (context, module, function, _, _) = parse(
        r#"module {
  %f = func.func @f(%x: !i32) -> !i32 {
    %one = constant {value = 1} : !i32
    %identity = muli %one, %one : !i32
    %y = muli %x, %identity : !i32
    func.return %y
  }
  module_end
}"#,
    );
    let before = execute(&context, function, 32, -17);
    EqSatPass::new()
        .with_iteration_limit(std::num::NonZeroUsize::new(1).expect("positive budget"))
        .run(function, &context, &AnalysisManager::new())
        .expect("budget stop keeps a finite result");
    assert_eq!(execute(&context, function, 32, -17), before);
    tir::verify_op_tree(&context, module.id()).expect("budgeted publication verifies");
}

#[test]
fn congruent_operands_match_a_repeated_pdl_binder() {
    let library = tir_pdl::compile(
        r#"
rule mul-one: builtin.muli(x: int<W>, 1) => x proof trusted;
rule idempotent: builtin.andi(x: int<W>, x) => x proof trusted;
"#,
    )
    .expect("valid typed library");
    let (context, module, function, _, _) = parse(
        r#"module {
  %f = func.func @f(%x: !i32, %y: !i32) -> !i32 {
    %a = muli %x, %y : !i32
    %b = muli %x, %y : !i32
    %same = andi %a, %b : !i32
    func.return %same
  }
  module_end
}"#,
    );
    EqSatPass::with_library(library)
        .expect("supported library")
        .run(function, &context, &AnalysisManager::new())
        .expect("congruent values match one binder");
    let text = printed_ir(&context, &module);
    assert_eq!(text.matches("muli ").count(), 1, "{text}");
    assert!(!text.contains("andi "), "{text}");
    tir::verify_op_tree(&context, module.id()).expect("reuse remains valid");
    for (x, y) in [(-7, 3), (5, 4)] {
        let result = interp::run_function(
            &context,
            function,
            vec![
                Value::Int(APInt::new_signed(32, x)),
                Value::Int(APInt::new_signed(32, y)),
            ],
        )
        .expect("rewritten expression executes");
        assert_eq!(result[0].to_i64(), Some(x * y));
    }
}

#[test]
fn equality_assumptions_publish_only_the_taken_scope_and_preserve_execution() {
    for (width, three, one) in [
        (1, "3", "1"),
        (8, "259", "257"),
        (32, "3", "1"),
        (32, "2", "1"),
        (128, "3", "1"),
    ] {
        let source = r#"module {
  %f = func.func @main(%x: !i32) -> !i32 {
    %three = constant {value = 3} : !i32
    %one = constant {value = 1} : !i32
    %p = cmpi %x, %three {predicate = "eq"} : !i1
    %r = scf.switch %p args(%x, %one) (%a0, %m0) {
      %v0 = ori %a0, %m0 : !i32
      -> %v0
    }
    (%a1, %m1) {
      %v1 = ori %a1, %m1 : !i32
      %out = ori %v1, %m1 : !i32
      -> %out
    }
    func.return %r
  }
  module_end
}
"#
        .replace("!i32", &format!("!i{width}"))
        .replace("value = 3", &format!("value = {three}"))
        .replace("value = 1", &format!("value = {one}"));
        let (context, module, function, _, _) = parse(&source);
        let body = context.get_region(context.get_op(function).regions()[0]);
        let block = body.iter(context.clone()).next().expect("entry block");
        let switch = context.get_op(block.op_ids()[3]);
        let arms = switch.regions();
        let false_arm = context.get_region(arms[0]);
        let true_arm = context.get_region(arms[1]);
        let false_yield = false_arm.results();
        let original_true = true_arm.results();
        let values = [-17, 0, 2, 3, 7];
        let before: Vec<_> = values
            .iter()
            .map(|&x| execute(&context, function, width, x))
            .collect();
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .expect("colored eqsat publishes");
        assert_eq!(
            false_arm.results(),
            false_yield,
            "sibling keeps its expression"
        );
        assert_ne!(
            true_arm.results(),
            original_true,
            "taken branch changes its yield"
        );
        let expected = if three == "2" {
            context.get_op(true_arm.op_ids()[0]).results()[0]
        } else {
            true_arm.ports()[0].id()
        };
        assert_eq!(true_arm.results(), [expected]);
        for (x, expected) in values.into_iter().zip(before) {
            assert_eq!(execute(&context, function, width, x), expected);
        }
        tir::verify_op_tree(&context, module.id()).expect("scoped publication verifies");
    }
    let original = include_str!("../../../../core/checks/Inputs/eqsat/branch_join_no_fold.tir");
    for identity in [false, true] {
        let source = if identity {
            original.replace(
                "%r = muli %c, %y : !i1",
                "%one = constant {value = 1} : !i1\n    %r = muli %c, %one : !i1",
            )
        } else {
            original.to_owned()
        };
        let (context, module, function, _, condition) = parse(&source);
        let body = context.get_region(context.get_op(function).regions()[0]);
        let join = body.iter(context.clone()).last().expect("join block");
        let ret = *join.op_ids().last().expect("join return");
        for round in 0..=1 {
            if round > 0 {
                EqSatPass::new()
                    .run(function, &context, &AnalysisManager::new())
                    .expect("default eqsat runs on the CFG join");
                tir::verify_op_tree(&context, module.id()).expect("published CFG verifies");
                if identity {
                    assert_eq!(
                        context.get_op(ret).operands().as_slice(),
                        [condition],
                        "mul-one publishes at the CFG join"
                    );
                }
            }
            for (c, y, product) in [
                (0, Some(0), Some(0)),
                (0, Some(1), Some(0)),
                (1, Some(0), Some(0)),
                (1, Some(1), Some(1)),
                (0, None, None),
                (1, None, None),
            ] {
                let result = interp::run_function(
                    &context,
                    function,
                    vec![
                        Value::Int(APInt::new(1, c)),
                        y.map_or(Value::Poison, |y| Value::Int(APInt::new(1, y))),
                    ],
                )
                .expect("CFG join executes");
                let expected = if identity { Some(c) } else { product };
                match (result.as_slice(), expected) {
                    ([Value::Int(actual)], Some(expected)) => {
                        assert_eq!(actual.with_signed(false), APInt::new(1, expected));
                    }
                    ([Value::Poison], None) => {}
                    _ => panic!(
                        "unexpected CFG result {result:?}, identity {identity}, c {c}, y {y:?}, round {round}"
                    ),
                }
            }
            assert!(matches!(
                interp::run_function(
                    &context,
                    function,
                    vec![Value::Poison, Value::Int(APInt::new(1, 1))],
                ),
                Err(interp::InterpError::Undefined(_))
            ));
        }
    }
}

#[test]
fn nested_scopes_inherit_equalities_without_changing_parent_or_sibling_uses() {
    let (context, module, function, ret, _) = parse(
        r#"module {
  %f = func.func @f(%x: !i32) -> !i32 {
    %three = constant {value = 3} : !i32
    %one = constant {value = 1} : !i32
    %p = cmpi %x, %three {predicate = "eq"} : !i1
    %r = scf.switch %p args(%x, %one) (%a0, %m0) {
      %v0 = ori %a0, %m0 : !i32
      -> %v0
    }
    (%a1, %m1) {
      %np = cmpi %a1, %m1 {predicate = "ne"} : !i1
      %n = scf.switch %np args(%a1, %m1) (%n0, %k0) {
        %nv0 = ori %n0, %k0 : !i32
        -> %nv0
      }
      (%n1, %k1) {
        %nv1 = ori %n1, %k1 : !i32
        -> %nv1
      }
      -> %n
    }
    %base = ori %x, %one : !i32
    %sum = addi %r, %base : !i32
    func.return %sum
  }
  module_end
}"#,
    );
    let body = context.get_region(context.get_op(function).regions()[0]);
    let block = body.iter(context.clone()).next().unwrap();
    let outer = context.get_op(block.op_ids()[3]);
    let sibling = context.get_region(outer.regions()[0]);
    let sibling_yield = sibling.results();
    let parent = context.get_region(outer.regions()[1]);
    let nested = context.get_op(parent.op_ids()[1]);
    let base = context.get_op(block.op_ids()[4]);
    let base_operands = base.operands();
    let ret_operands = context.get_op(ret).operands();
    let before: Vec<_> = [2, 3, 7]
        .into_iter()
        .map(|x| execute(&context, function, 32, x))
        .collect();
    let poison_before = interp::run_function(&context, function, vec![Value::Poison]);
    assert!(
        poison_before.is_err(),
        "a poison selector cannot choose a branch"
    );
    let mut pass = EqSatPass::new();
    pass.run(function, &context, &AnalysisManager::new())
        .expect("nested scopes publish");
    assert_eq!(sibling.results(), sibling_yield);
    assert_eq!(base.operands(), base_operands);
    assert_eq!(context.get_op(ret).operands(), ret_operands);
    for &region in nested.regions().iter() {
        let arm = context.get_region(region);
        assert_eq!(arm.results(), [arm.ports()[0].id()]);
    }
    for (x, expected) in [2, 3, 7].into_iter().zip(before) {
        assert_eq!(execute(&context, function, 32, x), expected);
    }
    assert_eq!(
        interp::run_function(&context, function, vec![Value::Poison]).is_err(),
        poison_before.is_err()
    );
    pass.run(function, &context, &AnalysisManager::new())
        .expect("fresh nested scopes run again");
    tir::verify_op_tree(&context, module.id()).unwrap();
}

#[test]
fn scoped_pass_preserves_caller_colors_and_rules_and_refreshes_constant_facts() {
    let (context, _, function, ret, input) = fixture(32, 1, "");
    let (caller, marker, seven) = {
        let mut engine = context.egraph_with_rules().unwrap();
        let commands = engine
            .parse_program(
                "caller",
                r#"(relation CallerMarker (i64))
          (relation CallerResult (i64)) (ruleset caller)
          (rule ((CallerMarker x)) ((CallerResult (+ x 1))) :ruleset caller)"#,
            )
            .unwrap();
        engine.run_program(commands).unwrap();
        engine.set_ruleset_colored("caller");
        let db = engine.database_mut();
        let caller = db.color(&[]);
        let marker = db.table("CallerMarker").unwrap();
        let seven = db.base(7_i64);
        db.insert_in(caller, marker, &[seven]).unwrap();
        (caller, marker, seven)
    };
    let mut pass = EqSatPass::new();
    pass.run(function, &context, &AnalysisManager::new())
        .unwrap();
    assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
    {
        let mut engine = context.egraph_with_rules().unwrap();
        engine.step_rules("caller").unwrap();
        let db = engine.database();
        assert_eq!(db.rows_in(caller, marker).next().unwrap()[0], seven);
        let result = db.table("CallerResult").unwrap();
        let eight = db.base(8_i64);
        assert_eq!(db.rows_in(caller, result).next().unwrap()[0], eight);
        assert_eq!(db.rows(result).count(), 0);
    }
    // Changing raw input after a run must not reuse old base numeric facts.
    let (context, module, function, ret, _) = fixture(64, 2, "");
    let body = context.get_region(context.get_op(function).regions()[0]);
    let block = body.iter(context.clone()).next().unwrap();
    let literal = block.op_ids()[0];
    context.set_op_attributes(function, {
        let mut attributes = context.get_op(function).attributes();
        attributes.push(context.named_attribute(
            "anchor",
            tir::attributes::AttributeValue::Value(context.get_op(literal).results()[0]),
        ));
        attributes
    });
    let old = context.get_op(ret).operands();
    context.set_op_attributes(function, {
        let mut attributes = context.get_op(function).attributes();
        attributes.push(context.named_attribute(
            "retained_expression",
            tir::attributes::AttributeValue::Value(old[0]),
        ));
        attributes
    });
    pass.run(module.id(), &context, &AnalysisManager::new())
        .unwrap();
    let constant = block.op_ids()[0];
    context.set_op_attributes(
        constant,
        vec![context.named_attribute("value", tir::attributes::AttributeValue::Int(1))],
    );
    pass.run(module.id(), &context, &AnalysisManager::new())
        .unwrap();
    assert_ne!(context.get_op(ret).operands(), old);
    context.set_op_attributes(
        constant,
        vec![context.named_attribute("value", tir::attributes::AttributeValue::Int(2))],
    );
    context.set_op_operands(ret, old.to_vec());
    let expected = execute(&context, function, 64, 7);
    pass.run(module.id(), &context, &AnalysisManager::new())
        .unwrap();
    assert_eq!(context.get_op(ret).operands(), old);
    assert_eq!(execute(&context, function, 64, 7), expected);
}

#[test]
fn known_bit_transfers_and_the_guarded_or_rule_are_proved_at_checked_widths() {
    use tir::sem::{ProofOutcome, SemGraph, SmtOracle};
    use tir_symbolic::{
        lang::SymKind,
        sem::{con, copy_subgraph, op, sym},
    };
    for width in [1, 8, 16, 32, 64, 128] {
        let source = format!(
            r#"module {{
          %f = func.func @f(%x: !i{width}, %mask: !i{width}) -> !i{width} {{
            %value = ori %x, %mask : !i{width}
            func.return %value
          }}
          module_end
        }}"#
        );
        let (context, _, function, _, _) = parse(&source);
        let body = context.get_region(context.get_op(function).regions()[0]);
        let block = body.iter(context.clone()).next().unwrap();
        let operation = context.get_op(block.op_ids()[0]).as_dyn_op();
        let mut declared = SemGraph::new();
        let declared_root = operation
            .semantic_expr(&mut declared)
            .expect("ori declares value semantics");
        assert!(operation.poison_expr(&mut SemGraph::new()).is_none());
        assert!(operation.requires_expr(&mut SemGraph::new()).is_none());
        for operands in [
            vec![Value::Poison, Value::Int(APInt::new(u64::from(width), 0))],
            vec![Value::Int(APInt::new(u64::from(width), 0)), Value::Poison],
            vec![Value::Poison, Value::Poison],
        ] {
            assert!(matches!(
                interp::run_function(&context, function, operands)
                    .unwrap()
                    .as_slice(),
                [Value::Poison]
            ));
        }
        for zeros in [false, true] {
            let mut lhs = SemGraph::new();
            let x = sym(&mut lhs, 0);
            let y = sym(&mut lhs, 1);
            let mx = sym(&mut lhs, 2);
            let my = sym(&mut lhs, 3);
            let px = if zeros {
                op(&mut lhs, SymKind::Not, &[x])
            } else {
                x
            };
            let py = if zeros {
                op(&mut lhs, SymKind::Not, &[y])
            } else {
                y
            };
            let kx = op(&mut lhs, SymKind::And, &[px, mx]);
            let ky = op(&mut lhs, SymKind::And, &[py, my]);
            let known = op(
                &mut lhs,
                if zeros { SymKind::And } else { SymKind::Or },
                &[kx, ky],
            );
            let result = copy_subgraph(&mut lhs, &declared, declared_root, &mut Default::default());
            let held = op(&mut lhs, SymKind::And, &[result, known]);
            let expected = if zeros {
                con(&mut lhs, 0, width)
            } else {
                known
            };
            op(&mut lhs, SymKind::Eq, &[held, expected]);
            let mut rhs = SemGraph::new();
            con(&mut rhs, 1, 1);
            assert_eq!(
                SmtOracle.equivalent_outcome(&lhs, &rhs, &[width; 4]),
                ProofOutcome::Proven,
                "known-zero={zeros}, i{width}"
            );
        }
        let mut lhs = SemGraph::new();
        let x = sym(&mut lhs, 0);
        let mask = sym(&mut lhs, 1);
        let subset = sym(&mut lhs, 2);
        let known = op(&mut lhs, SymKind::And, &[x, subset]);
        let intersection = op(&mut lhs, SymKind::And, &[known, mask]);
        let guard = op(&mut lhs, SymKind::Eq, &[intersection, mask]);
        let value = copy_subgraph(&mut lhs, &declared, declared_root, &mut Default::default());
        op(&mut lhs, SymKind::If, &[guard, value, x]);
        let mut rhs = SemGraph::new();
        sym(&mut rhs, 0);
        assert_eq!(
            SmtOracle.equivalent_outcome(&lhs, &rhs, &[width; 3]),
            ProofOutcome::Proven,
            "guard i{width}"
        );
    }
}

#[test]
fn boolean_gamma_assumptions_follow_the_reachable_true_arm() {
    let mut outcomes = Vec::new();
    for arms in [1, 3] {
        let regions = (0..arms)
            .map(|i| {
                format!(
                    r#"(%a{i}, %m{i}) {{
          %v{i} = ori %a{i}, %m{i} : !i32
          -> %v{i}
        }}"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let source = format!(
            r#"module {{
          %f = func.func @f(%x: !i32) -> !i32 {{
            %three = constant {{value = 3}} : !i32
            %one = constant {{value = 1}} : !i32
            %p = cmpi %x, %three {{predicate = "eq"}} : !i1
            %r = scf.switch %p args(%x, %one) {regions}
            func.return %r
          }}
          module_end
        }}"#
        );
        let (context, _, function, _, _) = parse(&source);
        let body = context.get_region(context.get_op(function).regions()[0]);
        let block = body.iter(context.clone()).next().unwrap();
        let gamma = context.get_op(block.op_ids()[3]);
        let original: Vec<_> = gamma
            .regions()
            .iter()
            .map(|&r| context.get_region(r).results())
            .collect();
        let before: Vec<_> = [2, 3]
            .into_iter()
            .map(|x| execute(&context, function, 32, x))
            .collect();
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        let changed = gamma
            .regions()
            .iter()
            .enumerate()
            .map(|(i, &region)| context.get_region(region).results() != original[i])
            .collect::<Vec<_>>();
        let after: Vec<_> = [2, 3]
            .into_iter()
            .map(|x| execute(&context, function, 32, x))
            .collect();
        outcomes.push((arms, changed, before, after));
    }
    assert_eq!(
        outcomes,
        vec![
            (
                1,
                vec![false],
                vec![APInt::new(32, 3); 2],
                vec![APInt::new(32, 3); 2]
            ),
            (
                3,
                vec![false, true, false],
                vec![APInt::new(32, 3); 2],
                vec![APInt::new(32, 3); 2]
            ),
        ]
    );
}

#[test]
fn unchanged_carried_results_use_the_entry_value_after_the_loop() {
    let original = include_str!("../../../../core/checks/Inputs/eqsat/theta_carried_port.tir");
    let mut pass = EqSatPass::new();
    for (changed, early_yield) in [(false, false), (true, false), (true, true)] {
        let source = if changed {
            original.replace(
                "scf.yield %5, %7",
                if early_yield {
                    "scf.yield %7, %7\n      scf.yield %5, %7"
                } else {
                    "scf.yield %7, %7"
                },
            )
        } else {
            original.to_owned()
        };
        let (context, module, function, ret, _) = parse(&source);
        let sum = context.get_op(
            context
                .get_value(context.get_op(ret).operands()[0])
                .defining_op()
                .unwrap(),
        );
        let owner = context
            .get_op(context.get_value(sum.operands()[0]).defining_op().unwrap())
            .as_operation(&tir::scf::ORDERED_FOR)
            .unwrap();
        let initial = tir::scf::ordered_for_inits(&owner)[0];
        let results = owner.results();
        for round in 0..3 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                assert_eq!(
                    sum.operands().as_slice(),
                    [if changed { results[1] } else { initial }, results[2]],
                    "unchanged result must use its entry value; changed={changed}, round={round}"
                );
                assert_eq!(owner.results(), results, "the advancing loop remains");
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            for (lb, ub, step, iterations) in [
                (0, 0, 1, 0),
                (0, 1, 1, 1),
                (0, 3, 1, 3),
                (2, 2, 3, 0),
                (2, 3, 3, 1),
                (2, 9, 3, 3),
            ] {
                let arguments = [lb, ub, step]
                    .map(|n| Value::Int(APInt::new(64, n)))
                    .to_vec();
                let actual = interp::run_function(&context, function, arguments).unwrap();
                let expected = if changed && iterations > 0 {
                    2 * iterations
                } else {
                    7 + iterations
                };
                let [Value::Int(actual)] = actual.as_slice() else {
                    panic!("expected carried sum")
                };
                assert_eq!(
                    actual.with_signed(false),
                    APInt::new(32, expected),
                    "round={round}, bounds={lb}/{ub}/{step}"
                );
            }
        }
    }
}

#[test]
fn loop_carried_ports_do_not_inherit_initial_value_facts() {
    for (operator, initial) in [("muli", "%unused"), ("muli", "%three"), ("ori", "%three")] {
        let increment = if operator == "muli" {
            "%identity"
        } else {
            "%i"
        };
        let source = format!(
            r#"module {{
  %f = func.func @f(%unused: !i32) -> !i32 {{
    %three = constant {{value = 3}} : !i32
    %one = constant {{value = 1}} : !i32
    %five = constant {{value = 5}} : !i32
    %result = scf.loop (%i = {initial}) {{
      %identity = {operator} %i, %one : !i32
      %next = addi {increment}, %one : !i32
      %again = cmpi %next, %five {{predicate = "slt"}} : !i1
      -> %again, %next, %identity
    }}
    func.return %result
  }}
  module_end
}}"#
        );
        let (context, module, function, _, _) = parse(&source);
        let expected = if operator == "ori" { 5 } else { 4 };
        let before = execute(&context, function, 32, 3);
        assert_eq!(before, APInt::new(32, expected));
        let body = context.get_region(context.get_op(function).regions()[0]);
        let owner = body
            .op_ids()
            .into_iter()
            .find(|&op| context.get_op(op).is_operation(&tir::scf::LOOP))
            .expect("carried loop");
        let region = context.get_region(context.get_op(owner).regions()[0]);
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .expect("loop input remains supported");
        let after = execute(&context, function, 32, 3);
        if operator == "muli" {
            assert_eq!(region.results()[2], region.ports()[0].id());
        }
        assert_eq!(
            after, before,
            "the current carried value differs from its initial value"
        );
        tir::verify_op_tree(&context, module.id()).expect("loop output verifies");
    }

    for (source, cases) in [
        (
            include_str!("../../../../core/checks/Inputs/eqsat/loop_counter_not_invariant.tir"),
            vec![(vec![(32, 0)], 1), (vec![(32, 1)], 1), (vec![(32, 4)], 4)],
        ),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/theta_hypothesis_refuted.tir"),
            vec![
                (vec![(64, 0), (64, 0), (64, 1)], 7),
                (vec![(64, 0), (64, 1), (64, 1)], 8),
                (vec![(64, 0), (64, 3), (64, 1)], 10),
                (vec![(64, 2), (64, 2), (64, 3)], 7),
                (vec![(64, 2), (64, 3), (64, 3)], 8),
                (vec![(64, 2), (64, 9), (64, 3)], 10),
            ],
        ),
    ] {
        let (context, module, function, _, _) = parse(source);
        let mut pass = EqSatPass::new();
        for round in 0..=2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .expect("original advancing loop is supported");
            }
            tir::verify_op_tree(&context, module.id()).expect("advancing loop verifies");
            for (inputs, expected) in &cases {
                let arguments = inputs
                    .iter()
                    .map(|&(width, value)| Value::Int(APInt::new(width, value)))
                    .collect();
                let result = interp::run_function(&context, function, arguments)
                    .expect("advancing loop executes");
                let [Value::Int(actual)] = result.as_slice() else {
                    panic!("expected advancing loop result")
                };
                assert_eq!(
                    actual.clone().with_signed(false),
                    APInt::new(32, *expected),
                    "inputs {inputs:?}, round {round}"
                );
            }
        }
    }
}

#[test]
fn one_pass_simplifies_multiple_functions_in_either_order() {
    const SOURCE: &str = r#"module {
  %f = func.func @narrow(%x: !i32) -> !i32 {
    %one = constant {value = 1} : !i32
    %result = muli %x, %one : !i32
    func.return %result
  }
  %g = func.func @wide(%x: !i64) -> !i64 {
    %one = constant {value = 1} : !i64
    %result = muli %x, %one : !i64
    func.return %result
  }
  %h = func.func @nonidentity(%x: !i8) -> !i8 {
    %two = constant {value = 2} : !i8
    %result = muli %x, %two : !i8
    func.return %result
  }
  module_end
}"#;
    for order in [[0, 1, 2], [2, 1, 0]] {
        let (context, module, _, _, _) = parse(SOURCE);
        let functions = context
            .get_region(context.get_op(module.id()).regions()[0])
            .op_ids();
        let cases: Vec<_> = functions
            .into_iter()
            .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
            .map(|function| {
                let body = context.get_region(context.get_op(function).regions()[0]);
                let block = body.iter(context.clone()).next().expect("entry block");
                let ret = *block.op_ids().last().expect("return");
                (
                    function,
                    ret,
                    block.arguments()[0].id(),
                    context.get_op(ret).operands()[0],
                )
            })
            .collect();
        assert_eq!(cases.len(), 3);
        let mut pass = EqSatPass::new();
        for index in order {
            pass.run(cases[index].0, &context, &AnalysisManager::new())
                .expect("each function simplifies");
        }
        for ((function, ret, input, original), (width, result)) in
            cases.into_iter().zip([(32, 7), (64, 7), (8, 14)])
        {
            let expected = if result == 7 { input } else { original };
            assert_eq!(context.get_op(ret).operands().as_slice(), [expected]);
            assert_eq!(
                execute(&context, function, width, 7),
                APInt::new(u64::from(width), result),
            );
        }
        tir::verify_op_tree(&context, module.id()).expect("all published functions verify");
    }
}

#[test]
fn unchanged_loop_carriers_publish_guarded_body_rewrites() {
    let mut pass = EqSatPass::new();
    for width in [32_u32, 1, 8, 16, 64, 128] {
        for changing in [false, true] {
            let feedback = if changing { "%changed" } else { "%x" };
            let source = format!(
                r#"module {{
  %f = func.func @f(%limit: !i32) -> !tuple<!i32, !i{width}> {{
    %zero = constant {{value = 0}} : !i32
    %step = constant {{value = 1}} : !i32
    %three = constant {{value = 3}} : !i{width}
    %one = constant {{value = 1}} : !i{width}
    %count, %value = scf.loop (%i = %zero, %x = %three) {{
      %next = addi %i, %step : !i32
      %again = cmpi %next, %limit {{predicate = "slt"}} : !i1
      %changed = addi %x, %one : !i{width}
      %redundant = ori %x, %one : !i{width}
      -> %again, %next, {feedback}, %next, %redundant
    }}
    %pair = make_tuple %count, %value : !tuple<!i32, !i{width}>
    func.return %pair
  }}
  module_end
}}"#
            );
            let (context, module, function, _, _) = parse(&source);
            let body = context.get_region(context.get_op(function).regions()[0]);
            let block = body.iter(context.clone()).next().unwrap();
            let loop_op = context.get_op(block.op_ids()[4]);
            let region = context.get_region(loop_op.regions()[0]);
            let port = region.ports()[1].id();
            let original = region.results();
            let evaluate = |limit| {
                execute_loop_pair(&context, function, vec![Value::Int(APInt::new(32, limit))])
            };
            // Theta is post-test, so limits zero and one both run one body.
            for limit in [0, 1, 4] {
                let count = limit.max(1);
                let value = if changing { (3 + count - 1) | 1 } else { 3 };
                assert_eq!(
                    evaluate(limit),
                    (APInt::new(32, count), APInt::new(width.into(), value))
                );
            }
            for _ in 0..2 {
                pass.run(function, &context, &AnalysisManager::new())
                    .expect("the same pass accepts another revision");
                let published = region.results()[4];
                if changing {
                    assert_eq!(published, original[4], "i{width}");
                } else {
                    assert_ne!(published, original[4], "i{width}");
                    let mask = context.get_op(block.op_ids()[3]).results()[0];
                    assert!(published == port || (width == 1 && published == mask));
                }
                assert_eq!(&region.results()[..4], &original[..4]);
                tir::verify_op_tree(&context, module.id()).expect("published loop verifies");
                for limit in [0, 1, 4] {
                    let count = limit.max(1);
                    let value = if changing { (3 + count - 1) | 1 } else { 3 };
                    assert_eq!(
                        evaluate(limit),
                        (APInt::new(32, count), APInt::new(width.into(), value))
                    );
                }
            }
        }
    }
}

fn execute_loop_pair(
    context: &Context,
    function: tir::OpId,
    arguments: Vec<Value>,
) -> (APInt, APInt) {
    let result = interp::run_function(context, function, arguments)
        .expect("the independent counter terminates");
    let [Value::Tuple(values)] = result.as_slice() else {
        panic!("expected both demanded loop results")
    };
    let [Value::Int(count), Value::Int(value)] = values.as_slice() else {
        panic!("expected integer counter and carrier")
    };
    (
        count.clone().with_signed(false),
        value.clone().with_signed(false),
    )
}

#[test]
fn unchanged_unknown_loop_carriers_preserve_poison_and_require_proved_bits() {
    let mut pass = EqSatPass::new();
    for width in [1_u32, 32, 128] {
        let source = format!(
            r#"module {{
  %f = func.func @f(%input: !i{width}) -> !tuple<!i32, !i{width}> {{
    %zero = constant {{value = 0}} : !i32
    %step = constant {{value = 1}} : !i32
    %four = constant {{value = 4}} : !i32
    %one = constant {{value = 1}} : !i{width}
    %count, %value = scf.loop (%i = %zero, %x = %input) {{
      %next = addi %i, %step : !i32
      %again = cmpi %next, %four {{predicate = "slt"}} : !i1
      %out = ori %x, %one : !i{width}
      -> %again, %next, %x, %next, %out
    }}
    %pair = make_tuple %count, %value : !tuple<!i32, !i{width}>
    func.return %pair
  }}
  module_end
}}"#
        );
        let (context, module, function, _, _) = parse(&source);
        let block = context
            .get_region(context.get_op(function).regions()[0])
            .op_ids();
        let loop_op = context.get_op(block[4]);
        let region = context.get_region(loop_op.regions()[0]);
        let original = region.results();
        for phase in ["before", "after"] {
            if phase == "after" {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                assert_eq!(region.results(), original, "no facts about i{width} input");
                tir::verify_op_tree(&context, module.id()).unwrap();
            }
            for input in [0, 2, 3] {
                assert_eq!(
                    execute_loop_pair(
                        &context,
                        function,
                        vec![Value::Int(APInt::new(width.into(), input))],
                    ),
                    (APInt::new(32, 4), APInt::new(width.into(), input | 1)),
                    "{phase}, i{width}"
                );
            }
            assert!(
                matches!(
                    interp::run_function(&context, function, vec![Value::Poison])
                        .expect("poison carrier preserves counter termination")
                        .as_slice(),
                    [Value::Poison]
                ),
                "{phase}, poison i{width}"
            );
        }
    }
}

#[test]
fn nested_loop_invariants_inherit_and_refresh_the_entry_values_scoped_facts() {
    let (context, module, function, _, _) = parse(
        r#"module {
  %f = func.func @f(%input: !i32) -> !tuple<!i32, !i32> {
    %zero = constant {value = 0} : !i32
    %one = constant {value = 1} : !i32
    %three = constant {value = 3} : !i32
    %four = constant {value = 4} : !i32
    %p = cmpi %input, %three {predicate = "eq"} : !i1
    %result = scf.switch %p args(%input) (%a0) {
      %c0, %v0 = scf.loop (%i0 = %zero, %x0 = %a0) {
        %next0 = addi %i0, %one : !i32
        %again0 = cmpi %next0, %four {predicate = "slt"} : !i1
        %out0 = ori %x0, %one : !i32
        -> %again0, %next0, %x0, %next0, %out0
      }
      %pair0 = make_tuple %c0, %v0 : !tuple<!i32, !i32>
      -> %pair0
    }
    (%a1) {
      %c1, %v1 = scf.loop (%i1 = %zero, %x1 = %a1) {
        %next1 = addi %i1, %one : !i32
        %again1 = cmpi %next1, %four {predicate = "slt"} : !i1
        %out1 = ori %x1, %one : !i32
        -> %again1, %next1, %x1, %next1, %out1
      }
      %pair1 = make_tuple %c1, %v1 : !tuple<!i32, !i32>
      -> %pair1
    }
    func.return %result
  }
  module_end
}"#,
    );
    let block = context
        .get_region(context.get_op(function).regions()[0])
        .op_ids();
    let gamma = context.get_op(block[5]);
    let loops: Vec<_> = gamma
        .regions()
        .iter()
        .map(|&arm| {
            let loop_op = context.get_op(context.get_region(arm).op_ids()[0]);
            let body = context.get_region(loop_op.regions()[0]);
            let original = body.results();
            (body, original)
        })
        .collect();
    context.set_op_attributes(function, {
        let mut attributes = context.get_op(function).attributes();
        for (index, (_, original)) in loops.iter().enumerate() {
            attributes.push(context.named_attribute(
                &format!("retained_expression_{index}"),
                tir::attributes::AttributeValue::Value(original[4]),
            ));
        }
        attributes
    });
    let mut pass = EqSatPass::new();
    for constant in [3, 2, 3] {
        context.set_op_attributes(
            block[2],
            vec![context.named_attribute("value", tir::attributes::AttributeValue::Int(constant))],
        );
        for (body, original) in &loops {
            context.set_region_results(body.id(), original.clone());
        }
        for phase in ["before", "after"] {
            if phase == "after" {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                assert_eq!(
                    loops[0].0.results(),
                    loops[0].1,
                    "sibling has no entry facts"
                );
                let expected = if constant == 3 {
                    loops[1].0.ports()[1].id()
                } else {
                    loops[1].1[4]
                };
                assert_eq!(loops[1].0.results()[4], expected, "entry={constant}");
                tir::verify_op_tree(&context, module.id()).unwrap();
            }
            for input in [2, 3, 6] {
                assert_eq!(
                    execute_loop_pair(&context, function, vec![Value::Int(APInt::new(32, input))]),
                    (APInt::new(32, 4), APInt::new(32, input | 1)),
                    "{phase}, entry={constant}, input={input}"
                );
            }
        }
    }
}

const TWO_CARRIER: &str = r#"module {
  %f = func.func @f(%limit: !i32) -> !tuple<!i32, !i32> {
    %zero = constant {value = 0} : !i32
    %step = constant {value = 1} : !i32
    %three = constant {value = 3} : !i32
    %one = constant {value = 1} : !i32
    %count, %value = scf.loop (%i = %zero, %x = %three) {
      %next = addi %i, %step : !i32
      %again = cmpi %next, %limit {predicate = "slt"} : !i1
      %changed = addi %x, %one : !i32
      %redundant = ori %x, %one : !i32
      -> %again, %next, %x, %next, %redundant
    }
    %pair = make_tuple %count, %value : !tuple<!i32, !i32>
    func.return %pair
  }
  module_end
}
"#;

fn printed_ir(context: &Context, module: &tir::OpHandle) -> String {
    let mut text = String::new();
    tir::print_ir(module, context, &mut tir::IRFormatter::new(&mut text)).unwrap();
    text
}

fn control_observations(context: &Context, function: tir::OpId) -> Vec<String> {
    let body = context.get_region(context.get_op(function).regions()[0]);
    let types = body.ports().iter().map(tir::Value::ty).collect::<Vec<_>>();
    let mut observations = Vec::new();
    for number in [0, 1, 7, -1] {
        for distinct in [false, true] {
            for poison in 0..=types.len() {
                let arguments = types
                    .iter()
                    .enumerate()
                    .map(|(index, &ty)| {
                        if index == poison {
                            Value::Poison
                        } else {
                            let bits = number + if distinct { index as i64 } else { 0 };
                            if let Some(integer) = tir::builtin::IntegerType::view(context, ty) {
                                Value::Int(APInt::new(integer.width(), bits as u64))
                            } else {
                                assert!(tir::ptr::PtrType::view(context, ty).is_some());
                                Value::Ptr(bits as u64)
                            }
                        }
                    })
                    .collect();
                let result = interp::run_function_within(context, function, arguments, Some(512));
                observations.push(match result {
                    Ok(values) => format!(
                        "{:?}",
                        values
                            .into_iter()
                            .map(|value| match value {
                                Value::Int(bits) => Value::Int(bits.with_signed(false)),
                                other => other,
                            })
                            .collect::<Vec<_>>()
                    ),
                    Err(interp::InterpError::Undefined(_)) => "Undefined".to_owned(),
                    other => format!("{other:?}"),
                });
            }
        }
    }
    observations
}

type FixtureExample = Option<(&'static [i64], i64)>;

const ORIGINAL_FIXTURES: &[(&str, &str, bool, FixtureExample)] = &[
    (
        "dse_late_address_congruence",
        include_str!("../../../../core/checks/Inputs/eqsat/dse_late_address_congruence.tir"),
        true,
        Some((&[0, 0], 4202)),
    ),
    (
        "gamma_arm_reads_nothing_downstream",
        include_str!("../../../../core/checks/Inputs/eqsat/gamma_arm_reads_nothing_downstream.tir"),
        true,
        Some((&[0, 7], 64)),
    ),
    (
        "dse_noalias_parameter_dies",
        include_str!("../../../../core/checks/Inputs/eqsat/dse_noalias_parameter_dies.tir"),
        false,
        None,
    ),
    (
        "gamma_load_arms_cross",
        include_str!("../../../../core/checks/Inputs/eqsat/gamma_load_arms_cross.tir"),
        true,
        Some((&[1], 1)),
    ),
    (
        "nodes-loop-invariant",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-loop-invariant.tir"),
        true,
        Some((&[4], 14)),
    ),
    (
        "slot_read_in_gate_region",
        include_str!("../../../../core/checks/Inputs/eqsat/slot_read_in_gate_region.tir"),
        true,
        Some((&[1, 7], 14)),
    ),
    (
        "dse_disjoint_read_fork",
        include_str!("../../../../core/checks/Inputs/eqsat/dse_disjoint_read_fork.tir"),
        false,
        None,
    ),
    (
        "dse_read_fork_off_a_read",
        include_str!("../../../../core/checks/Inputs/eqsat/dse_read_fork_off_a_read.tir"),
        true,
        None,
    ),
    (
        "load_cse_via_state",
        include_str!("../../../../core/checks/Inputs/eqsat/load_cse_via_state.tir"),
        true,
        None,
    ),
    (
        "cse_keeps_the_fork_join",
        include_str!("../../../../core/checks/Inputs/eqsat/cse_keeps_the_fork_join.tir"),
        true,
        None,
    ),
    (
        "rule_composition",
        include_str!("../../../../core/checks/Inputs/eqsat/rule_composition.tir"),
        false,
        None,
    ),
    (
        "while_body_forwarded_port",
        include_str!("../../../../core/checks/Inputs/eqsat/while_body_forwarded_port.tir"),
        true,
        Some((&[0, 9], 10)),
    ),
    (
        "theta_hypothesis_nested",
        include_str!("../../../../core/checks/Inputs/eqsat/theta_hypothesis_nested.tir"),
        true,
        Some((&[0, 3, 1], 14)),
    ),
    (
        "theta_hypothesis_break_refutes",
        include_str!("../../../../core/checks/Inputs/eqsat/theta_hypothesis_break_refutes.tir"),
        true,
        Some((&[0, 0, 13], 7)),
    ),
    (
        "theta_hypothesis_forwarded_exit",
        include_str!("../../../../core/checks/Inputs/eqsat/theta_hypothesis_forwarded_exit.tir"),
        false,
        Some((&[], 49)),
    ),
    (
        "theta_identity_forwarded_exit",
        include_str!("../../../../core/checks/Inputs/eqsat/theta_identity_forwarded_exit.tir"),
        false,
        Some((&[], 49)),
    ),
    (
        "gate_eq_operands",
        include_str!("../../../../core/checks/Inputs/eqsat/gate_eq_operands.tir"),
        true,
        Some((&[0, 0, 7, 19], 7)),
    ),
    (
        "gamma_arm_args",
        include_str!("../../../../core/checks/Inputs/eqsat/gamma_arm_args.tir"),
        true,
        Some((&[0, 13], 13)),
    ),
    (
        "module_value_forwarded",
        include_str!("../../../../core/checks/Inputs/eqsat/module_value_forwarded.tir"),
        false,
        None,
    ),
    (
        "slot_promote_through_alias",
        include_str!("../../../../core/checks/Inputs/eqsat/slot_promote_through_alias.tir"),
        true,
        Some((&[7, 13], 13)),
    ),
    (
        "uniform_pointer_promote",
        include_str!("../../../../core/checks/Inputs/eqsat/uniform_pointer_promote.tir"),
        true,
        None,
    ),
    (
        "gamma_decided_arm",
        include_str!("../../../../core/checks/Inputs/eqsat/gamma_decided_arm.tir"),
        true,
        Some((&[7], 13)),
    ),
    (
        "gate_decided",
        include_str!("../../../../core/checks/Inputs/eqsat/gate_decided.tir"),
        true,
        None,
    ),
    (
        "gate_eq_literal",
        include_str!("../../../../core/checks/Inputs/eqsat/gate_eq_literal.tir"),
        true,
        None,
    ),
    (
        "guarded_region",
        include_str!("../../../../core/checks/Inputs/eqsat/guarded_region.tir"),
        true,
        None,
    ),
    (
        "dead_store_across_call",
        include_str!("../../../../core/checks/Inputs/eqsat/dead_store_across_call.tir"),
        false,
        None,
    ),
    (
        "dead_store_memset",
        include_str!("../../../../core/checks/Inputs/eqsat/dead_store_memset.tir"),
        false,
        None,
    ),
    (
        "fp-flags-cse",
        include_str!("../../../../core/checks/Inputs/eqsat/fp-flags-cse.tir"),
        false,
        None,
    ),
    (
        "state_chain_not_bypassed",
        include_str!("../../../../core/checks/Inputs/eqsat/state_chain_not_bypassed.tir"),
        false,
        None,
    ),
    (
        "store_forward_across_call",
        include_str!("../../../../core/checks/Inputs/eqsat/store_forward_across_call.tir"),
        false,
        None,
    ),
    (
        "store_forward_memset",
        include_str!("../../../../core/checks/Inputs/eqsat/store_forward_memset.tir"),
        false,
        None,
    ),
    (
        "theta_load_condition_latched_escaping_keep",
        include_str!(
            "../../../../core/checks/Inputs/eqsat/theta_load_condition_latched_escaping_keep.tir"
        ),
        false,
        None,
    ),
    (
        "theta_load_early_exit_escaping_keep",
        include_str!(
            "../../../../core/checks/Inputs/eqsat/theta_load_early_exit_escaping_keep.tir"
        ),
        false,
        None,
    ),
    (
        "nodes-identity-feedback-variant-exit",
        include_str!(
            "../../../../core/checks/Inputs/eqsat/nodes-identity-feedback-variant-exit.tir"
        ),
        true,
        Some((&[1, 2], 51)),
    ),
    (
        "while_body_fact",
        include_str!("../../../../core/checks/Inputs/eqsat/while_body_fact.tir"),
        true,
        None,
    ),
    (
        "dead_store",
        include_str!("../../../../core/checks/Inputs/eqsat/dead_store.tir"),
        false,
        None,
    ),
    (
        "dead_store_pointer",
        include_str!("../../../../core/checks/Inputs/eqsat/dead_store_pointer.tir"),
        false,
        None,
    ),
    (
        "dead_store_load_between",
        include_str!("../../../../core/checks/Inputs/eqsat/dead_store_load_between.tir"),
        false,
        None,
    ),
    (
        "store_forward_midchain",
        include_str!("../../../../core/checks/Inputs/eqsat/store_forward_midchain.tir"),
        false,
        None,
    ),
    (
        "gamma_load_escaping_keep",
        include_str!("../../../../core/checks/Inputs/eqsat/gamma_load_escaping_keep.tir"),
        false,
        None,
    ),
    (
        "gamma_load_mixed_types_keep",
        include_str!("../../../../core/checks/Inputs/eqsat/gamma_load_mixed_types_keep.tir"),
        false,
        None,
    ),
    (
        "gamma_load_switch_escaping_keep",
        include_str!("../../../../core/checks/Inputs/eqsat/gamma_load_switch_escaping_keep.tir"),
        false,
        None,
    ),
    (
        "theta_load_invariant_slot_while",
        include_str!("../../../../core/checks/Inputs/eqsat/theta_load_invariant_slot_while.tir"),
        true,
        Some((&[0, 3, 7], 7)),
    ),
    (
        "theta_load_switch_arms",
        include_str!("../../../../core/checks/Inputs/eqsat/theta_load_switch_arms.tir"),
        true,
        Some((&[0, 1, 7], 7)),
    ),
    (
        "theta_load_escaping_keep",
        include_str!("../../../../core/checks/Inputs/eqsat/theta_load_escaping_keep.tir"),
        false,
        None,
    ),
    (
        "nodes-counter-advance-cse",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-counter-advance-cse.tir"),
        true,
        Some((&[3], 6)),
    ),
    (
        "nodes-loop-diverges",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-loop-diverges.tir"),
        true,
        None,
    ),
    (
        "nodes-for2-invariant",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-for2-invariant.tir"),
        true,
        Some((&[3], 10)),
    ),
    (
        "nodes-loop-kept",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-loop-kept.tir"),
        true,
        Some((&[3], 2)),
    ),
    (
        "nodes-loop-exit-variant",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-loop-exit-variant.tir"),
        true,
        Some((&[1, 2], 51)),
    ),
    (
        "cross_block",
        include_str!("../../../../core/checks/Inputs/eqsat/cross_block.tir"),
        true,
        Some((&[7], 14)),
    ),
    (
        "nodes-gate-cse",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-gate-cse.tir"),
        true,
        Some((&[1, 6], 15)),
    ),
    (
        "nodes-trap-in-arm",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-trap-in-arm.tir"),
        true,
        Some((&[0, 6, 0], 18)),
    ),
    (
        "dse_private_slot_across_call_dies",
        include_str!("../../../../core/checks/Inputs/eqsat/dse_private_slot_across_call_dies.tir"),
        false,
        None,
    ),
    (
        "nodes-forwarded-load-across-call",
        include_str!("../../../../core/checks/Inputs/eqsat/nodes-forwarded-load-across-call.tir"),
        false,
        None,
    ),
    (
        "dse_ptradd_chains_one_object",
        include_str!("../../../../core/checks/Inputs/eqsat/dse_ptradd_chains_one_object.tir"),
        true,
        Some((&[7, 13], 13)),
    ),
    (
        "slot_promote_through_ptradd",
        include_str!("../../../../core/checks/Inputs/eqsat/slot_promote_through_ptradd.tir"),
        false,
        None,
    ),
    (
        "theta_load_stored_two_gammas_deep",
        include_str!("../../../../core/checks/Inputs/eqsat/theta_load_stored_two_gammas_deep.tir"),
        false,
        None,
    ),
    (
        "fp-private-slot-across-trap",
        include_str!("../../../../core/checks/Inputs/eqsat/fp-private-slot-across-trap.tir"),
        false,
        None,
    ),
    (
        "fp-effects-dce",
        include_str!("../../../../core/checks/Inputs/eqsat/fp-effects-dce.tir"),
        false,
        None,
    ),
];

#[test]
fn original_fixtures_preserve_execution_and_require_their_output_contracts() {
    let mut failures = Vec::new();
    for &(name, source, observe, example) in ORIGINAL_FIXTURES {
        let context = Context::with_default_dialects();
        let module = parse_operation(&context, source, &MODULE).unwrap();
        tir::verify_op_tree(&context, module.id()).unwrap();
        let functions = context
            .get_region(context.get_op(module.id()).regions()[0])
            .op_ids()
            .into_iter()
            .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
            .collect::<Vec<_>>();
        let before = functions
            .iter()
            .filter(|_| observe)
            .map(|&function| control_observations(&context, function))
            .collect::<Vec<_>>();
        let mut pass = EqSatPass::new();
        for &function in &functions {
            let body = context.get_region(context.get_op(function).regions()[0]);
            let owners = body
                .op_ids()
                .into_iter()
                .map(|id| {
                    let op = context.get_op(id);
                    let regions = op
                        .regions()
                        .into_iter()
                        .map(|id| context.get_region(id))
                        .collect::<Vec<_>>();
                    (op, regions)
                })
                .collect::<Vec<_>>();
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            for (op, regions) in owners {
                if !op.is_live() {
                    assert!(
                        regions.iter().all(|region| !region.is_live()),
                        "erased owner retains regions in {name}"
                    );
                }
            }
        }
        tir::verify_op_tree(&context, module.id()).unwrap();
        for (&function, expected) in functions.iter().zip(before) {
            assert_eq!(control_observations(&context, function), expected, "{name}");
        }
        if let Some((inputs, expected)) = example {
            let body = context.get_region(context.get_op(functions[0]).regions()[0]);
            assert_eq!(inputs.len(), body.ports().len());
            let arguments = inputs
                .iter()
                .zip(body.ports())
                .map(|(&bits, value)| {
                    let width = tir::builtin::IntegerType::view(&context, value.ty())
                        .unwrap()
                        .width();
                    Value::Int(APInt::new_signed(width, bits))
                })
                .collect();
            let actual =
                interp::run_function_within(&context, functions[0], arguments, Some(512)).unwrap();
            let [Value::Int(value)] = actual.as_slice() else {
                panic!("expected integer result")
            };
            assert_eq!(
                value.clone().with_signed(true),
                APInt::new_signed(value.width(), expected)
            );
        }
        let output = printed_ir(&context, &module);
        if let Err(error) = filecheck::verify(
            &filecheck::Source::new(name, source),
            &filecheck::Source::new("optimized", &output),
            &filecheck::Config::default(),
        ) {
            failures.push(format!("{name}: {error}\n{output}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn conditional_feedback_preserves_poison_selector_demand_and_first_exit() {
    let source = r#"module {
  %f = func.func @f(%selector: !i1) -> !i32 {
    %seven = constant {value = 7} : !i32
    %zero = constant {value = 0} : !i32
    %one = constant {value = 1} : !i32
    %r, %finali = scf.loop (%carry = %seven, %i = %zero) {
      %condition = cmpi %i, %zero {predicate = "eq"} : !i1
      %nexti = addi %i, %one : !i32
      %feedback = scf.switch %selector { -> %carry } { -> %seven }
      -> %condition, %feedback, %nexti, %carry, %i
    }
    -> %r
  }
  module_end
}"#;
    for first_exit in [false, true] {
        let source = if first_exit {
            source.replace(
                "%i, %zero {predicate = \"eq\"}",
                "%carry, %seven {predicate = \"slt\"}",
            )
        } else {
            source.to_owned()
        };
        let context = Context::with_default_dialects();
        let module = parse_operation(&context, &source, &MODULE).unwrap();
        tir::verify_op_tree(&context, module.id()).unwrap();
        let function = context
            .get_region(context.get_op(module.id()).regions()[0])
            .op_ids()[0];
        for round in 0..=2 {
            if round > 0 {
                EqSatPass::new()
                    .run(function, &context, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&context, module.id()).unwrap();
            }
            for selector in [Some(0), Some(1), None] {
                let mut state = interp::ExecutionState::default();
                let external = state.memory.alloc(8, 1);
                state.memory.write(external, &[0x5a; 8]).unwrap();
                let argument = selector.map_or(Value::Poison, |v| Value::Int(APInt::new(1, v)));
                let actual = interp::run_function_in_state(
                    &context,
                    function,
                    vec![argument],
                    &mut state,
                    Some(128),
                );
                if selector.is_none() && !first_exit {
                    assert!(
                        matches!(actual, Err(interp::InterpError::Undefined(_))),
                        "round={round}, first_exit={first_exit}: {actual:?}"
                    );
                } else {
                    assert!(
                        matches!(actual.as_deref(), Ok([Value::Int(v)]) if v.with_signed(false) == APInt::new(32, 7)),
                        "round={round}, first_exit={first_exit}: {actual:?}"
                    );
                }
                assert_eq!(state.memory.read(external, 8).unwrap(), [0x5a; 8]);
            }
        }
    }
}

fn loop_shape(context: &Context, owner: tir::OpId) -> (usize, usize, usize, usize) {
    let owner = context.get_op(owner);
    let region = context.get_region(owner.regions()[0]);
    (
        owner.operands().len(),
        region.ports().len(),
        region.results().len(),
        owner.results().len(),
    )
}

#[test]
fn loop_program_preserves_results_across_invocations() {
    let mut pass = EqSatPass::new();
    for _ in 0..2 {
        let (context, module, function, _, _) = parse(TWO_CARRIER);
        let block = context
            .get_region(context.get_op(function).regions()[0])
            .op_ids();
        let owner = context.get_op(block[4]);
        let region = context.get_region(owner.regions()[0]);
        let caller = {
            let mut engine = context.egraph_with_rules().unwrap();
            let commands = engine
                .parse_program(
                    "carrier caller",
                    r#"
                (relation CarrierMarker (i64))
                (relation CarrierReadback (i64)) (ruleset carrier-caller)
                (rule ((CarrierMarker x)) ((CarrierReadback (+ x 1))) :ruleset carrier-caller)
            "#,
                )
                .unwrap();
            engine.run_program(commands).unwrap();
            engine.set_ruleset_colored("carrier-caller");
            let db = engine.database_mut();
            let caller = db.color(&[]);
            db.insert_in(
                caller,
                db.table("CarrierMarker").unwrap(),
                &[db.base(7_i64)],
            )
            .unwrap();
            caller
        };
        eprintln!("before replacement:\n{}", printed_ir(&context, &module));
        for limit in [0, 1, 4] {
            assert_eq!(
                execute_loop_pair(&context, function, vec![Value::Int(APInt::new(32, limit))]),
                (APInt::new(32, limit.max(1)), APInt::new(32, 3))
            );
        }
        for revision in 0..3 {
            if revision == 1 {
                context.set_region_results(region.id(), region.results());
            }
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            for limit in [0, 1, 4] {
                assert_eq!(
                    execute_loop_pair(&context, function, vec![Value::Int(APInt::new(32, limit))]),
                    (APInt::new(32, limit.max(1)), APInt::new(32, 3))
                );
            }
        }
        {
            let mut engine = context.egraph_with_rules().unwrap();
            engine.step_rules("carrier-caller").unwrap();
            let db = engine.database();
            let marker = db.table("CarrierMarker").unwrap();
            let readback = db.table("CarrierReadback").unwrap();
            assert_eq!(
                db.rows_in(caller, marker).next().unwrap()[0],
                db.base(7_i64)
            );
            assert_eq!(
                db.rows_in(caller, readback).next().unwrap()[0],
                db.base(8_i64)
            );
            assert_eq!(db.rows(readback).count(), 0);
        }
        eprintln!("after replacement:\n{}", printed_ir(&context, &module));
    }
}

#[test]
fn caller_libraries_and_partial_loop_values_preserve_behavior() {
    let mut pass = EqSatPass::new();
    for (name, source) in [
        ("caller library", TWO_CARRIER.to_owned()),
        (
            "changing feedback",
            TWO_CARRIER.replace("%next, %x, %next", "%next, %changed, %next"),
        ),
        (
            "unknown entry",
            TWO_CARRIER.replace("%x = %three", "%x = %limit"),
        ),
        (
            "partial entry",
            TWO_CARRIER
                .replace(
                    "    %count, %value",
                    "    %partial = divsi %three, %zero : !i32\n    %count, %value",
                )
                .replace("%x = %three", "%x = %partial"),
        ),
        (
            "unsupported flags",
            TWO_CARRIER.replace("addi %i, %step :", "addi %i, %step {nsw = true} :"),
        ),
        (
            "no independent demand",
            TWO_CARRIER.replace("make_tuple %count, %value", "make_tuple %value, %value"),
        ),
        (
            "effect",
            TWO_CARRIER
                .replace(
                    "    %count, %value",
                    "    %address = ptr.alloca {size = 4, align = 4} : !ptr.p\n    %count, %value",
                )
                .replace(
                    "      %next =",
                    "      ptr.store %x, %address\n      %next =",
                ),
        ),
    ] {
        let (context, module, function, _, _) = parse(&source);
        let owner = context
            .get_region(context.get_op(function).regions()[0])
            .op_ids()
            .into_iter()
            .find(|&op| context.get_op(op).is_operation(&tir::scf::LOOP))
            .unwrap();
        let before_ir = printed_ir(&context, &module);
        let before = [0, 1, 4].map(|limit| {
            interp::run_function(&context, function, vec![Value::Int(APInt::new(32, limit))])
                .map(|values| format!("{values:?}"))
                .map_err(|error| format!("{error:?}"))
        });
        if name == "caller library" {
            let library =
                tir_pdl::compile("rule own: builtin.muli(x: int<32>, 1) => x proof trusted;")
                    .unwrap();
            EqSatPass::with_library(library)
                .unwrap()
                .run(function, &context, &AnalysisManager::new())
                .unwrap();
            assert_eq!(printed_ir(&context, &module), before_ir);
        } else {
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
        }
        assert_eq!(loop_shape(&context, owner), (2, 2, 5, 2), "{name}");
        tir::verify_op_tree(&context, module.id()).unwrap();
        let after = [0, 1, 4].map(|limit| {
            interp::run_function(&context, function, vec![Value::Int(APInt::new(32, limit))])
                .map(|values| format!("{values:?}"))
                .map_err(|error| format!("{error:?}"))
        });
        assert_eq!(after, before, "{name}");
    }
}

#[test]
fn unsupported_value_metadata_keeps_its_referenced_carrier() {
    let (context, module, function, _, _) = parse(TWO_CARRIER);
    let block = context
        .get_region(context.get_op(function).regions()[0])
        .op_ids();
    let owner = context.get_op(block[4]);
    let region = context.get_region(owner.regions()[0]);
    let local = region.ports()[1].id();
    let result = owner.results()[1];
    for value in [local, result] {
        let mut attrs = owner.attributes();
        attrs.push(context.named_attribute(
            "capture",
            tir::attributes::AttributeValue::Array(
                vec![tir::attributes::AttributeValue::Value(value)].into_boxed_slice(),
            ),
        ));
        context.set_op_attributes(owner.id, attrs);
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        assert_eq!(loop_shape(&context, owner.id), (2, 2, 5, 2));
        assert_eq!(
            owner.attr("capture"),
            Some(tir::attributes::AttributeValue::Array(
                vec![tir::attributes::AttributeValue::Value(value)].into_boxed_slice()
            ))
        );
        tir::verify_op_tree(&context, module.id()).unwrap();
        context.set_op_attributes(
            owner.id,
            owner
                .attributes()
                .into_iter()
                .filter(|attr| attr.name != context.intern("capture"))
                .collect(),
        );
    }
}

tir::helpers::op_def! {
    file: "tests/inputs/eqsat-carrier-exit.cdsl",
    hosts: {
        carrier_exit_interface => (NonLocalExit, &tir::NonLocalExitCallbacks {
            target: |_| tir::ExitTarget::InnermostLoop,
            values: |operation| operation.operands().to_vec(),
        }),
    },
}

tir::helpers::dialect! {
    CarrierExitDialect {
        name: "eqsat_test",
        operations: [CARRIER_EXIT],
        types: [],
    }
}

#[test]
fn nonlocal_exits_keep_their_complete_loop_result_binding() {
    let (context, module, function, _, _) = parse(TWO_CARRIER);
    context.register_dialect::<CarrierExitDialect>();
    let block = context
        .get_region(context.get_op(function).regions()[0])
        .op_ids();
    let owner = context.get_op(block[4]);
    let region = context.get_region(owner.regions()[0]);
    let values = region
        .ports()
        .iter()
        .map(tir::Value::id)
        .collect::<Vec<_>>();
    let operands = values
        .iter()
        .map(|value| format!("%{}", tir::RowId::row(*value)))
        .collect::<Vec<_>>()
        .join(", ");
    let exit =
        tir::parse::ir::parse_op(&context, &format!("eqsat_test.carrier_exit {operands}")).unwrap();
    context.insert(tir::InsertionPoint::Region(region.id()), exit.id());
    assert_eq!(
        tir::analysis::exits::resolve_exit_target(&context, exit.id()).unwrap(),
        owner.id
    );
    tir::verify_op_tree(&context, module.id()).unwrap();
    EqSatPass::new()
        .run(function, &context, &AnalysisManager::new())
        .unwrap();
    assert_eq!(loop_shape(&context, owner.id), (2, 2, 5, 2));
    assert_eq!(exit.operands().as_slice(), values);
    tir::verify_op_tree(&context, module.id()).unwrap();
}

#[test]
fn ordered_nonlocal_exits_keep_the_carried_result() {
    let context = Context::with_default_dialects();
    context.register_dialect::<CarrierExitDialect>();
    let source = include_str!("../../../../core/checks/Inputs/eqsat/theta_carried_port.tir")
        .replace(
            "scf.yield %5, %7",
            "eqsat_test.carrier_exit %i1, %7, %7\n      scf.yield %5, %7",
        );
    let module = parse_operation(&context, &source, &MODULE).unwrap();
    tir::verify_op_tree(&context, module.id()).unwrap();
    let function = context.get_region(module.regions()[0]).op_ids()[0];
    let body = context.get_region(context.get_op(function).regions()[0]);
    let owner = context.get_op(body.op_ids()[3]);
    let sum = context.get_op(body.op_ids()[4]);
    let original = sum.operands();
    let region = context.get_region(owner.regions()[0]);
    let exit = region.op_ids()[1];
    assert_eq!(
        tir::analysis::exits::resolve_exit_target(&context, exit).unwrap(),
        owner.id()
    );
    let mut pass = EqSatPass::new();
    for _ in 0..2 {
        pass.run(function, &context, &AnalysisManager::new())
            .unwrap();
        assert_eq!(sum.operands(), original);
        tir::verify_op_tree(&context, module.id()).unwrap();
    }
}

fn rhs_engine(context: &Context, function: tir::OpId, source: &str) {
    // Register production primitives without applying the default rule library.
    EqSatPass::with_library(tir_pdl::File { items: Vec::new() })
        .unwrap()
        .run(function, context, &AnalysisManager::new())
        .unwrap();
    let library = tir_pdl::compile(source).expect("valid trusted PDL");
    let program = tir_pdl::emit_egglog(&library, "Rhs").unwrap().source;
    eprintln!("compiled semantic program:\n{program}");
    let mut engine = context.egraph_with_rules().unwrap();
    let commands = engine.parse_program("recursive RHS", &program).unwrap();
    engine.run_program(commands).unwrap();
    let commands = engine
        .parse_program(
            "imported roots",
            r#"
        (RhsSeed 0)
        (ruleset rhs-test-seed)
        (rule ((= kind (OpKind op))) ((RhsUse 0 op)) :ruleset rhs-test-seed)
        (run-schedule rhs-test-seed)
        (run-schedule (saturate Rhsproject))
        (run-schedule (repeat 12 (seq Rhsrewrite Rhsproject)))
    "#,
        )
        .unwrap();
    engine.run_program(commands).unwrap();
}

fn rhs_constructor(context: &Context, kind: &str) -> String {
    let engine = context.egraph();
    let db = engine.database();
    let table = db.table("RhsConstructor").unwrap();
    let matches: Vec<_> = db
        .rows(table)
        .filter(|row| db.base_value::<String>(row[1]) == kind)
        .collect();
    assert_eq!(matches.len(), 1, "one constructor for {kind}");
    assert_eq!(db.base_value::<i64>(matches[0][2]), 2);
    db.base_value(matches[0][0])
}

fn rhs_check(context: &Context, assertion: &str, equal: bool) {
    let mut engine = context.egraph();
    let commands = engine
        .parse_program("semantic equality", assertion)
        .unwrap();
    let result = engine.run_program(commands);
    assert_eq!(result.is_ok(), equal, "{assertion}: {result:?}");
}

#[test]
fn recursive_rhs_saturates_imported_terms_and_composes() {
    for repeated in [false, true] {
        let (context, _, function, ret, input) = parse(
            &r#"module {
      %f = func.func @f(%x: !i32, %y: !i32) -> !i32 {
        %three = constant {value = 3} : !i32
        %a = muli %x, %three : !i32
        %b = muli %y, %three : !i32
        %sum = addi %a, %b : !i32
        func.return %sum
      }
      module_end
    }"#
            .replace(
                "%b = muli %y",
                if repeated {
                    "%b = muli %x"
                } else {
                    "%b = muli %y"
                },
            ),
        );
        let root = context.get_op(ret).operands()[0].number();
        rhs_engine(
            &context,
            function,
            r#"
      rule factor: builtin.addi(builtin.muli(x: int<W>, c: const<W>), builtin.muli(y: int<W>, c)) => builtin.muli(builtin.addi(x,y),c) proof trusted;
      rule double: builtin.addi(x: int<W>, x) => builtin.muli(x,2) proof trusted;
    "#,
        );
        let mul = rhs_constructor(&context, "builtin.muli");
        let add = rhs_constructor(&context, "builtin.addi");
        let ty = "(Apply (Ctor \"builtin.i\") (TCons (NatLit (from-string \"32\")) (TNil)))";
        let body = context.get_region(context.get_op(function).regions()[0]);
        let block = body.iter(context.clone()).next().unwrap();
        let y = format!(
            "(RhsInput {})",
            if repeated {
                input
            } else {
                block.arguments()[1].id()
            }
            .number()
        );
        let x = format!("(RhsInput {})", input.number());
        rhs_check(
            &context,
            &format!(
                "(check (= (RhsInput {root}) ({mul} {ty} ({add} {ty} {x} {y}) (RhsConstant {ty} \"3\"))))"
            ),
            true,
        );
        rhs_check(
            &context,
            &format!(
                "(check (= (RhsInput {root}) ({mul} {ty} ({mul} {ty} {x} (RhsConstant {ty} \"2\")) (RhsConstant {ty} \"3\"))))"
            ),
            repeated,
        );
    }
}
#[test]
fn rhs_literal_bits_and_repeated_binders_use_actual_types() {
    for (width, negative_bits) in [
        (1, "1"),
        (8, "ff"),
        (32, "ffffffff"),
        (64, "ffffffffffffffff"),
        (128, "ffffffffffffffffffffffffffffffff"),
    ] {
        for nested_negative in [false, true] {
            for (operation, attributes, binder_width, equal) in [
                ("addi %x, %x", "", width, true),
                ("addi %x, %one", "", width, false),
                ("addi %x, %x", "{nsw = true}", width, false),
                ("addi %x, %x", "", width + 1, false),
            ] {
                let source = format!(
                    r#"module {{
                  %f = func.func @f(%x: !i{width}) -> !i{width} {{
                    %one = constant {{value = 1}} : !i{width}
                    %sum = {operation} {attributes} : !i{width}
                    func.return %sum
                  }}
                  module_end
                }}"#
                );
                let (context, _, function, ret, input) = parse(&source);
                let root = context.get_op(ret).operands()[0].number();
                let rhs = if nested_negative {
                    "builtin.muli(x,builtin.subi(1,-1))"
                } else {
                    "builtin.muli(x,2)"
                };
                rhs_engine(
                    &context,
                    function,
                    &format!(
                        "rule double: builtin.addi(x: int<{binder_width}>, x) => {rhs} proof trusted;"
                    ),
                );
                let mul = rhs_constructor(&context, "builtin.muli");
                let ty = format!(
                    "(Apply (Ctor \"builtin.i\") (TCons (NatLit (from-string \"{width}\")) (TNil)))"
                );
                let coefficient = if nested_negative {
                    let sub = rhs_constructor(&context, "builtin.subi");
                    format!(
                        "({sub} {ty} (RhsConstant {ty} \"1\") (RhsConstant {ty} \"{negative_bits}\"))"
                    )
                } else {
                    let bits = if width == 1 { "0" } else { "2" };
                    format!("(RhsConstant {ty} \"{bits}\")")
                };
                rhs_check(
                    &context,
                    &format!(
                        "(check (= (RhsInput {root}) ({mul} {ty} (RhsInput {}) {coefficient})))",
                        input.number()
                    ),
                    equal,
                );
            }
        }
    }
}

#[test]
fn rhs_unsupported_contracts_remain_errors() {
    for rhs in [
        "builtin.trunci(x, x, x)",
        "builtin.addi(x, 2) : int<W>",
        "builtin.addi(x, 2 | x)",
        "builtin.addi<nsw = 1>(x, 2)",
        "const<W>(2)",
        "const<W>(2 + 2)",
        "builtin.muli(x, -(1 + 1))",
        "#add(x, 2)",
    ] {
        let source =
            format!("rule unsupported: builtin.addi(x: int<W>, x) => {rhs} proof trusted;");
        let library = tir_pdl::compile(&source).expect("unsupported RHS has valid PDL syntax");
        assert!(tir_pdl::emit_egglog(&library, "Rhs").is_err(), "{rhs}");
    }
    let base =
        tir_pdl::compile("rule negative: builtin.addi(x: int<W>, x) => x proof trusted;").unwrap();
    for case in 0..7 {
        let mut library = base.clone();
        let tir_pdl::Item::Rule(rule) = &mut library.items[0] else {
            unreachable!()
        };
        match case {
            0 => rule.kind = tir_pdl::RuleKind::Equality(tir_pdl::Direction::Bidirectional),
            1 => rule.kind = tir_pdl::RuleKind::Refinement,
            2 => rule.proof = Some(tir_pdl::Proof::Definitional),
            3 => rule.post_saturation = true,
            4 => rule
                .requirements
                .push(tir_pdl::RefinementRequirement::SameRoundRegion),
            5 => {
                rule.rhs.kind = tir_pdl::TermKind::Binder {
                    name: "missing".into(),
                    ty: None,
                }
            }
            _ => {
                rule.rhs.kind = tir_pdl::TermKind::Binder {
                    name: "x".into(),
                    ty: Some(tir_pdl::BindingType::Type(tir_pdl::Type::Integer(
                        tir_pdl::Width::Any,
                    ))),
                }
            }
        }
        assert!(
            tir_pdl::emit_egglog(&library, "Rhs").is_err(),
            "contract case {case}"
        );
    }
}

#[test]
fn rhs_construction_preserves_constant_guards() {
    for other in [7, 8] {
        let (context, _, function, ret, _) = parse(&format!(
            r#"module {{
          %f = func.func @f(%unused: !i8) -> !i8 {{
            %a = constant {{value = 7}} : !i8
            %b = constant {{value = {other}}} : !i8
            %sum = addi %a, %b : !i8
            func.return %sum
          }}
          module_end
        }}"#
        ));
        let root = context.get_op(ret).operands()[0].number();
        rhs_engine(
            &context,
            function,
            "rule guarded: builtin.addi(x: const<W>, y: const<W>) => builtin.muli(x,2) where x == y proof trusted;",
        );
        let mul = rhs_constructor(&context, "builtin.muli");
        let ty = "(Apply (Ctor \"builtin.i\") (TCons (NatLit (from-string \"8\")) (TNil)))";
        rhs_check(
            &context,
            &format!(
                "(check (= (RhsInput {root}) ({mul} {ty} (RhsConstant {ty} \"7\") (RhsConstant {ty} \"2\"))))"
            ),
            other == 7,
        );
    }
}

#[test]
fn rhs_family_requires_declared_safe_integer_interfaces() {
    for kind in ["builtin.divsi", "builtin.cmpi", "scf.unknown"] {
        let (context, _, function, _, _) = fixture(32, 2, "");
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        let library = tir_pdl::compile(&format!(
            "rule unsupported: builtin.addi(x: int<W>, x) => {kind}(x,x) proof trusted;"
        ))
        .unwrap();
        let program = tir_pdl::emit_egglog(&library, "Rhs").unwrap().source;
        let mut engine = context.egraph_with_rules().unwrap();
        let commands = engine
            .parse_program("unadmitted RHS kind", &program)
            .unwrap();
        assert!(engine.run_program(commands).is_err(), "{kind}");
    }
}

const FACTOR_LIBRARY: &str = "rule factor: builtin.addi(builtin.muli(x: int<W>, c: const<W>), builtin.muli(y: int<W>, c)) => builtin.muli(builtin.addi(x,y),c) proof trusted;";

#[test]
fn caller_library_publishes_closed_scalar_factoring() {
    let source = r#"module {
      %f = func.func @f(%x: !i32, %y: !i32) -> !i32 {
        %three = constant {value = 3} : !i32
        %a = muli %x, %three : !i32
        %b = muli %y, %three : !i32
        %sum = addi %a, %b : !i32
        func.return %sum
      }
      module_end
    }"#;
    let (context, module, function, _, _) = parse(source);
    let mut pass = EqSatPass::with_library(tir_pdl::compile(FACTOR_LIBRARY).unwrap()).unwrap();
    pass.run(function, &context, &AnalysisManager::new())
        .unwrap();
    let text = printed_ir(&context, &module);
    assert_eq!(text.matches("muli ").count(), 1, "{text}");
    assert_eq!(text.matches("addi ").count(), 1, "{text}");
    for input in [-7, 0, i32::MAX as i64] {
        let result = scalar_results(&context, function, input, Some(11));
        assert_eq!(
            result,
            vec![APInt::new(
                32,
                (input as u32).wrapping_add(11).wrapping_mul(3) as u64
            )]
        );
    }
}

fn scalar_results(context: &Context, function: tir::OpId, x: i64, y: Option<i64>) -> Vec<APInt> {
    let arguments = std::iter::once(x)
        .chain(y)
        .map(|v| Value::Int(APInt::new_signed(32, v)))
        .collect();
    let results =
        interp::run_function(context, function, arguments).expect("scalar function executes");
    let values = match results.as_slice() {
        [Value::Tuple(values)] => values.as_slice(),
        values => values,
    };
    values
        .iter()
        .map(|value| match value {
            Value::Int(value) => value.clone().with_signed(false),
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect()
}

const DOUBLE_LIBRARY: &str =
    "rule double: builtin.addi(x: int<W>, x) => builtin.muli(x,2) proof trusted;";

#[test]
fn scalar_collection_retains_duplicates_and_removes_unsafe_dead_slots() {
    for late in [false, true] {
        let extra = if late {
            "%late = addi %x, %y : !i32"
        } else {
            ""
        };
        let rhs = if late { "%late" } else { "%y" };
        let third = if late { "%r" } else { "%a" };
        let source = format!(
            r#"module {{
          %f = func.func @f(%x: !i32, %y: !i32) -> !tuple<!i32, !i32, !i32> {{
            %c = constant {{value = 3}} : !i32
            %a = muli %x, %c : !i32
            {extra}
            %b = muli {rhs}, %c : !i32
            %r = addi %a, %b : !i32
            %tuple = make_tuple %r, %r, {third} : !tuple<!i32, !i32, !i32>
            %dead0 = addi %x, %y : !i32
            %dead1 = addi %dead0, %c : !i32
            func.return %tuple
          }}
          module_end
        }}"#
        );
        let (context, module, function, _, _) = parse(&source);
        let before = printed_ir(&context, &module);
        let body = context.get_region(context.get_op(function).regions()[0]);
        let old = body.op_ids();
        EqSatPass::with_library(tir_pdl::compile(FACTOR_LIBRARY).unwrap())
            .unwrap()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        let after = printed_ir(&context, &module);
        eprintln!("closed scalar late={late}\nBEFORE\n{before}AFTER\n{after}");
        assert_eq!(
            body.op_ids().len(),
            old.len() - 2 - usize::from(late),
            "{after}"
        );
        if late {
            assert_eq!(after.matches("muli ").count(), 1, "{after}");
        }
        assert!(body.op_ids().iter().all(|op| old.contains(op)));
        for x in [-7, 0, i32::MAX as i64] {
            let a = (x as u32).wrapping_mul(3);
            let r = (x as u32)
                .wrapping_add(if late {
                    (x as u32).wrapping_add(11)
                } else {
                    11
                })
                .wrapping_mul(3);
            assert_eq!(
                scalar_results(&context, function, x, Some(11)),
                [r, r, if late { r } else { a }].map(|v| APInt::new(32, v.into()))
            );
        }
    }
}

#[test]
fn selected_composed_constants_publish_with_weighted_equal_size_costs() {
    let library = tir_pdl::compile(&format!("{FACTOR_LIBRARY}\n{DOUBLE_LIBRARY}")).unwrap();
    let mut pass = EqSatPass::with_library(library)
        .unwrap()
        .with_operation_cost(|kind| if kind == "builtin.addi" { 10 } else { 1 });
    for _ in 0..2 {
        let (context, module, function, ret, input) = parse(
            r#"module {
          %f = func.func @f(%x: !i32) -> !i32 {
            %c = constant {value = 3} : !i32
            %a = muli %x, %c : !i32
            %b = muli %x, %c : !i32
            %r = addi %a, %b : !i32
            func.return %r
          }
          module_end
        }"#,
        );
        let before = printed_ir(&context, &module);
        for round in 0..2 {
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
            let after = printed_ir(&context, &module);
            eprintln!("composed round={round}\nBEFORE\n{before}AFTER\n{after}");
            assert_eq!(after.matches("muli ").count(), 2, "{after}");
            assert_eq!(after.matches("constant ").count(), 2, "{after}");
            assert!(!after.contains("addi "), "{after}");
            assert!(after.contains("value = 2"), "{after}");
            let outer = context.get_op(
                context
                    .get_value(context.get_op(ret).operands()[0])
                    .defining_op()
                    .unwrap(),
            );
            let three = context.get_op(
                context
                    .get_value(outer.operands()[1])
                    .defining_op()
                    .unwrap(),
            );
            assert_eq!(
                three.attr("value"),
                Some(tir::attributes::AttributeValue::Int(3))
            );
            let inner = context.get_op(
                context
                    .get_value(outer.operands()[0])
                    .defining_op()
                    .unwrap(),
            );
            assert_eq!(inner.operands()[0], input);
            let two = context.get_op(
                context
                    .get_value(inner.operands()[1])
                    .defining_op()
                    .unwrap(),
            );
            assert_eq!(
                two.attr("value"),
                Some(tir::attributes::AttributeValue::Int(2))
            );
            for x in [-7, 0, i32::MAX as i64] {
                assert_eq!(
                    execute(&context, function, 32, x),
                    APInt::new(32, (x as u32).wrapping_mul(6).into())
                );
            }
            assert!(matches!(
                interp::run_function(&context, function, vec![Value::Poison])
                    .unwrap()
                    .as_slice(),
                [Value::Poison]
            ));
        }
        let body = context.get_region(context.get_op(function).regions()[0]);
        let block = body.iter(context.clone()).next().unwrap();
        let one = tir::parse::ir::parse_op(&context, "constant {value = 1} : !i32").unwrap();
        context.insert(
            tir::InsertionPoint::Block {
                block: block.id(),
                position: block.op_ids().iter().position(|&op| op == ret).unwrap(),
            },
            one.id(),
        );
        assert!(block.op_ids().contains(&one.id()));
        pass.run(function, &context, &AnalysisManager::new())
            .unwrap();
        assert_eq!(
            execute(&context, function, 32, -7),
            APInt::new(32, (-42_i32) as u32 as u64)
        );
        tir::verify_op_tree(&context, module.id()).unwrap();
    }
}

#[test]
fn sharing_cost_and_safe_capacity_keep_the_original_finite_cube() {
    for (weighted, dead) in [(false, false), (true, false), (true, true)] {
        let after_consumer = if dead {
            "%dead0 = addi %x, %x : !i32\n%dead1 = addi %dead0, %x : !i32"
        } else {
            ""
        };
        let source = format!(
            r#"module {{
          %f = func.func @f(%x: !i32) -> !tuple<!i32, !i32, !i32> {{
            %square = muli %x, %x : !i32
            %cube = muli %square, %x : !i32
            %twice = addi %cube, %cube : !i32
            %tuple = make_tuple %twice, %twice, %cube : !tuple<!i32, !i32, !i32>
            {after_consumer}
            func.return %tuple
          }}
          module_end
        }}"#
        );
        let (context, module, function, _, _) = parse(&source);
        let before = printed_ir(&context, &module);
        let mut pass = EqSatPass::with_library(tir_pdl::compile(DOUBLE_LIBRARY).unwrap())
            .unwrap()
            .with_operation_cost(move |kind| {
                if weighted && kind == "builtin.addi" {
                    10
                } else {
                    1
                }
            });
        pass.run(function, &context, &AnalysisManager::new())
            .unwrap();
        assert_eq!(
            printed_ir(&context, &module),
            before,
            "weighted={weighted}, dead={dead}"
        );
        for x in [-7, 0, i32::MAX as i64] {
            let cube = (x as u32).wrapping_pow(3);
            assert_eq!(
                scalar_results(&context, function, x, None),
                [cube.wrapping_mul(2), cube.wrapping_mul(2), cube]
                    .map(|v| APInt::new(32, v.into()))
            );
        }
    }
}

#[test]
fn weighted_cube_choice_publishes_despite_equal_physical_row_counts() {
    let (context, module, function, _, _) = parse(
        r#"module {
      %f = func.func @f(%x: !i32) -> !tuple<!i32, !i32> {
        %square = muli %x, %x : !i32
        %cube = muli %square, %x : !i32
        %twice = addi %cube, %cube : !i32
        %two = constant {value = 2} : !i32
        %tuple = make_tuple %twice, %two : !tuple<!i32, !i32>
        func.return %tuple
      }
      module_end
    }"#,
    );
    let before = printed_ir(&context, &module);
    EqSatPass::with_library(tir_pdl::compile(DOUBLE_LIBRARY).unwrap())
        .unwrap()
        .with_operation_cost(|kind| if kind == "builtin.addi" { 10 } else { 1 })
        .run(function, &context, &AnalysisManager::new())
        .unwrap();
    let after = printed_ir(&context, &module);
    eprintln!("weighted cube\nBEFORE\n{before}AFTER\n{after}");
    assert_eq!(after.matches("muli ").count(), 3, "{after}");
    assert_eq!(after.matches("constant ").count(), 1, "{after}");
    assert!(!after.contains("addi "), "{after}");
    for x in [-7, 0, i32::MAX as i64] {
        assert_eq!(
            scalar_results(&context, function, x, None),
            [(x as u32).wrapping_pow(3).wrapping_mul(2), 2].map(|v| APInt::new(32, v.into()))
        );
    }
}

#[test]
fn scalar_factoring_uses_one_shared_graph_collection_without_layout_order() {
    for multi in [false, true] {
        let context = Context::with_default_dialects();
        let (ret_type, tuple, extra, output) = if multi {
            (
                "!tuple<!i32, !i32, !i32>",
                "%tuple = make_tuple %sum, %sum, %extra : !tuple<!i32, !i32, !i32>",
                "%extra = muli %x, %c : !i32",
                "%tuple",
            )
        } else {
            ("!i32", "", "", "%sum")
        };
        let source = format!(
            r#"module {{
  %f = func.func @graph(%x: !i32, %y: !i32) -> {ret_type} {{
    {tuple}
    %sum = addi %a, %b : !i32
    {extra}
    %a = muli %x, %c : !i32
    %c = constant {{value = 3}} : !i32
    %b = muli %y, %c : !i32
    -> {output}
  }}
  module_end
}}"#
        );
        let module = parse_operation(&context, &source, &MODULE).unwrap();
        let function = context.get_op(context.get_region(module.regions()[0]).op_ids()[0]);
        let body = context.get_region(function.regions()[0]);
        let before = body.op_ids();
        EqSatPass::with_library(tir_pdl::compile(FACTOR_LIBRARY).unwrap())
            .unwrap()
            .run(function.id(), &context, &AnalysisManager::new())
            .unwrap();
        assert_eq!(body.op_ids().len(), before.len() - 1);
        assert!(body.op_ids().iter().all(|op| before.contains(op)));
        for x in [-7, 0, i32::MAX as i64] {
            let r = (x as u32).wrapping_add(11).wrapping_mul(3);
            let expected = if multi {
                vec![r, r, (x as u32).wrapping_mul(3)]
            } else {
                vec![r]
            };
            assert_eq!(
                scalar_results(&context, function.id(), x, Some(11)),
                expected
                    .into_iter()
                    .map(|v| APInt::new(32, v.into()))
                    .collect::<Vec<_>>()
            );
        }
        tir::verify_op_tree(&context, function.id()).unwrap();
    }
}

#[test]
fn closed_scalar_scopes_reject_metadata_and_require_a_semantic_improvement() {
    const SOURCE: &str = r#"module {
      %f = func.func @f(%x: !i32, %y: !i32) -> !i32 {
        %c = constant {value = 3} : !i32
        %a = muli %x, %c : !i32
        %b = muli %y, %c : !i32
        %r = addi %a, %b : !i32
        func.return %r
      }
      module_end
    }"#;
    const NO_EQUATION: &str = "rule add: builtin.addi(x: int<W>,y: int<W>) => builtin.addi(x,y) proof trusted; rule mul: builtin.muli(x: int<W>,y: int<W>) => builtin.muli(x,y) proof trusted;";
    for control in [
        "no-equation",
        "flags",
        "mixed",
        "global",
        "assigned",
        "block",
        "root",
        "nested",
        "blocks",
        "effect",
    ] {
        let source = match control {
            "flags" => SOURCE.replace("addi %a, %b :", "addi %a, %b {nsw = true} :"),
            "mixed" => SOURCE.replace("%y: !i32", "%y: !i64").replace("muli %y, %c", "muli %x, %c"),
            "nested" => SOURCE.replace(
                "func.return %r",
                "%ignored = scf.loop (%i = %r) {\n %p = cmpi %i, %i {predicate = \"ne\"} : !i1\n -> %p, %i, %i\n}\nfunc.return %r",
            ),
            "blocks" => SOURCE.replace("func.return %r", "func.return %r\n^unused:\nfunc.return %x"),
            "effect" => SOURCE
                .replace(
                    "%f = func.func",
                    "%ext = func.declare @ext(!i32) -> !i32\n%f = func.func",
                )
                .replace(
                    "func.return %r",
                    "%ignored = func.call %ext(%r : !i32) -> !i32\nfunc.return %r",
                ),
            _ => SOURCE.to_owned(),
        };
        let context = Context::with_default_dialects();
        let module = parse_operation(&context, &source, &MODULE).unwrap();
        let functions = context
            .get_region(context.get_op(module.id()).regions()[0])
            .op_ids();
        let function = functions[usize::from(control == "effect")];
        let body = context.get_region(context.get_op(function).regions()[0]);
        let block = body.iter(context.clone()).next().unwrap();
        let value = context.get_op(block.op_ids()[1]).results()[0];
        if matches!(control, "global" | "assigned") {
            let owner = *functions.last().unwrap();
            let attribute = if control == "assigned" {
                context.register_reg_classes(&super::fixtures::R_CLASSES);
                tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Assigned {
                    value,
                    class: super::fixtures::r(),
                    index: 0,
                })
            } else {
                tir::attributes::AttributeValue::Array(
                    vec![tir::attributes::AttributeValue::Value(value)].into(),
                )
            };
            context.set_op_attributes(owner, vec![context.named_attribute("hidden", attribute)]);
        } else if control == "block" {
            block.set_attr(
                "hidden",
                tir::attributes::AttributeValue::Array(
                    vec![tir::attributes::AttributeValue::Value(value)].into(),
                ),
            );
        } else if control == "root" {
            let mut attributes = context.get_op(function).attributes();
            attributes.push(
                context.named_attribute("unsupported", tir::attributes::AttributeValue::Bool(true)),
            );
            context.set_op_attributes(function, attributes);
        }
        tir::verify_op_tree(&context, module.id()).unwrap();
        let before = printed_ir(&context, &module);
        let library = if control == "no-equation" {
            NO_EQUATION
        } else {
            FACTOR_LIBRARY
        };
        EqSatPass::with_library(tir_pdl::compile(library).unwrap())
            .unwrap()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        if control == "mixed" {
            assert_eq!(printed_ir(&context, &module).matches("= muli ").count(), 1);
            let actual = interp::run_function(
                &context,
                function,
                vec![
                    Value::Int(APInt::new_signed(32, -7)),
                    Value::Int(APInt::new_signed(64, 11)),
                ],
            )
            .unwrap();
            assert_eq!(actual[0].to_i64(), Some(-42));
        } else if control == "blocks" {
            assert_eq!(printed_ir(&context, &module).matches("= muli ").count(), 1);
        } else {
            assert_eq!(printed_ir(&context, &module), before, "{control}");
        }
        if !matches!(control, "mixed" | "effect") {
            assert_eq!(
                scalar_results(&context, function, -7, Some(11)),
                [APInt::new(32, 12)]
            );
        }
    }
}

#[test]
fn scalar_cost_bounds_and_zero_weights_preserve_source_and_cleanup() {
    for weight in [0, i64::MAX as u64, u64::MAX] {
        let (context, module, function, _, _) = parse(
            r#"module {
          %f = func.func @f(%x: !i32) -> !i32 {
            %zero = constant {value = 0} : !i32
            %two = constant {value = 2} : !i32
            %identity = addi %x, %zero : !i32
            %result = muli %identity, %two : !i32
            func.return %result
          }
          module_end
        }"#,
        );
        let before = printed_ir(&context, &module);
        let mut pass = EqSatPass::new().with_operation_cost(move |_| weight);
        let result = pass.run(function, &context, &AnalysisManager::new());
        assert_eq!(result.is_err(), weight == u64::MAX);
        assert_eq!(printed_ir(&context, &module), before);
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        assert_eq!(
            execute(&context, function, 32, -7),
            APInt::new(32, (-14_i32) as u32 as u64)
        );
    }
    let (context, _, function, ret, input) = fixture(32, 2, "");
    let body = context.get_region(context.get_op(function).regions()[0]);
    let constant = body.op_ids()[0];
    let mut pass = EqSatPass::new().with_operation_cost(|_| 1);
    pass.run(function, &context, &AnalysisManager::new())
        .unwrap();
    assert_ne!(context.get_op(ret).operands().as_slice(), [input]);
    context.set_op_attributes(
        constant,
        vec![context.named_attribute("value", tir::attributes::AttributeValue::Int(1))],
    );
    pass.run(function, &context, &AnalysisManager::new())
        .unwrap();
    assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
    assert!(!context.has_operation(constant));
    pass.run(function, &context, &AnalysisManager::new())
        .unwrap();
    assert_eq!(
        execute(&context, function, 32, -7),
        APInt::new(32, (-7_i32) as u32 as u64)
    );
}

#[test]
fn zero_node_closed_output_removes_an_unrelated_dead_pool() {
    let (context, module, function, ret, input) = parse(
        r#"module {
      %f = func.func @f(%x: !i32) -> !i32 {
        %dead = constant {value = 2} : !i32
        func.return %x
      }
      module_end
    }"#,
    );
    let mut pass = EqSatPass::new();
    for _ in 0..2 {
        pass.run(function, &context, &AnalysisManager::new())
            .unwrap();
        assert_eq!(context.get_op(ret).operands().as_slice(), [input]);
        let text = printed_ir(&context, &module);
        assert!(!text.contains("constant "), "{text}");
        assert_eq!(
            execute(&context, function, 32, -7),
            APInt::new(32, (-7_i32) as u32 as u64)
        );
    }
}

#[test]
fn division_by_one_fixtures_require_defined_dividends_and_remove_admitted_operations() {
    let mut unsimplified = Vec::new();
    for (fixture, source) in [
        include_str!("../../../../core/checks/Inputs/eqsat/div_rem_by_one.tir"),
        include_str!("../../../../core/checks/Inputs/eqsat/integer_sdiv_one.tir"),
        include_str!("../../../../core/checks/Inputs/eqsat/integer_srem_one.tir"),
        include_str!("../../../../core/checks/Inputs/eqsat/integer_udiv_one.tir"),
        include_str!("../../../../core/checks/Inputs/eqsat/integer_urem_one.tir"),
    ]
    .into_iter()
    .enumerate()
    {
        for defined in [false, true] {
            let source = if defined {
                source
                    .replace("divsi %x, %a", "divsi %a, %a")
                    .replace("divui %x, %a", "divui %a, %a")
                    .replace("divsi %x, %one", "divsi %one, %one")
                    .replace("divui %x, %one", "divui %one, %one")
                    .replace("remsi %x, %a", "remsi %a, %a")
                    .replace("remui %x, %a", "remui %a, %a")
                    .replace("remsi %x, %one", "remsi %one, %one")
                    .replace("remui %x, %one", "remui %one, %one")
            } else {
                source.to_owned()
            };
            let (context, module, _, _, _) = parse(&source);
            let functions = context
                .get_region(context.get_op(module.id()).regions()[0])
                .op_ids()
                .into_iter()
                .filter(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
                .collect::<Vec<_>>();
            let mut pass = EqSatPass::new();
            for function in functions {
                let body = context.get_region(context.get_op(function).regions()[0]);
                let block = body.iter(context.clone()).next().unwrap();
                let input = block.arguments()[0].id();
                let width =
                    tir::builtin::IntegerType::view(&context, context.get_value(input).ty())
                        .unwrap()
                        .width();
                let ret = *body.op_ids().last().unwrap();
                let operation = body.op_ids()[1];
                let remainder = context.get_op(operation).is_operation(&tir::builtin::REMSI)
                    || context.get_op(operation).is_operation(&tir::builtin::REMUI);
                for round in 0..3 {
                    if round > 0 {
                        pass.run(function, &context, &AnalysisManager::new())
                            .unwrap();
                        if !defined {
                            assert!(
                                body.op_ids().contains(&operation),
                                "unknown dividend may be poison"
                            );
                        } else if !remainder {
                            if context.has_operation(operation) || body.op_ids().len() != 2 {
                                unsimplified.push((fixture, remainder, width, round));
                            }
                        } else {
                            let result = context.get_op(
                                context
                                    .get_value(context.get_op(ret).value_operands()[0])
                                    .defining_op()
                                    .unwrap(),
                            );
                            if !result.is_operation(&tir::builtin::CONSTANT)
                                || (round == 2 && body.op_ids().len() != 2)
                            {
                                unsimplified.push((fixture, remainder, width, round));
                            } else {
                                assert_eq!(
                                    result
                                        .attr("value")
                                        .unwrap()
                                        .int_at(width)
                                        .unwrap()
                                        .with_signed(false),
                                    APInt::zero(width)
                                );
                            }
                        }
                    }
                    tir::verify_op_tree(&context, module.id()).unwrap();
                    for bits in [Some(-17), Some(-1), Some(0), Some(1), Some(19), None] {
                        let mut args =
                            vec![Value::Int(APInt::zero(width)); block.arguments().len()];
                        args[0] =
                            bits.map_or(Value::Poison, |n| Value::Int(APInt::new_signed(width, n)));
                        let result = interp::run_function(&context, function, args);
                        if bits.is_none() && !defined {
                            assert!(matches!(result, Err(interp::InterpError::Undefined(_))));
                            continue;
                        }
                        let result = result.unwrap();
                        let expected = if defined {
                            Some(if remainder { 0 } else { 1 })
                        } else {
                            bits.map(|n| if remainder { 0 } else { n })
                        };
                        match (result.as_slice(), expected) {
                            ([Value::Int(value)], Some(n)) => assert_eq!(
                                value.clone().with_signed(false),
                                APInt::new_signed(width, n).with_signed(false)
                            ),
                            ([Value::Poison], None) => {}
                            _ => panic!("unexpected result, round {round}: {result:?}"),
                        }
                    }
                }
            }
        }
    }
    assert!(
        unsimplified.is_empty(),
        "admitted operations must be physically removed: {unsimplified:?}"
    );
}

#[test]
fn division_by_one_guards_preserve_partiality_poison_attributes_and_widths() {
    for operator in ["divui", "divsi", "remui", "remsi"] {
        for (width, divisor, attributes) in [
            (1u32, 1, ""),
            (8, 1, ""),
            (16, 1, ""),
            (32, 1, ""),
            (64, 1, ""),
            (128, 1, ""),
            (8, 1, "{nsw = true}"),
            (8, 0, ""),
            (8, -1, ""),
            (8, 2, ""),
        ] {
            for defined in [false, true] {
                let dividend = if defined { "%known" } else { "%x" };
                let (context, module, function, ret, _) = parse(&format!(
                    r#"module {{
                  %f = func.func @f(%x: !i{width}) -> !i{width} {{
                    %known = constant {{value = -128}} : !i{width}
                    %divisor = constant {{value = {divisor}}} : !i{width}
                    %result = {operator} {dividend}, %divisor {attributes} : !i{width}
                    func.return %result
                  }}
                  module_end
                }}"#
                ));
                let operation = context
                    .get_value(context.get_op(ret).value_operands()[0])
                    .defining_op()
                    .unwrap();
                let original_attributes = context.get_op(operation).attributes();
                let remainder = operator.starts_with("rem");
                let signed = operator.ends_with("si");
                let simplify =
                    defined && divisor == 1 && !(signed && width == 1) && attributes.is_empty();
                let mut pass = EqSatPass::new();
                for round in 0..3 {
                    if round > 0 {
                        pass.run(function, &context, &AnalysisManager::new())
                            .unwrap();
                        let retained = context.has_operation(operation)
                            && context.get_op(operation).name().as_str() == operator;
                        assert_eq!(
                            retained,
                            !simplify,
                            "{operator}, i{width}, {divisor}, {attributes}, defined={defined}, round={round}\n{}",
                            printed_ir(&context, &module)
                        );
                        if !simplify {
                            assert_eq!(context.get_op(operation).attributes(), original_attributes);
                        }
                    }
                    tir::verify_op_tree(&context, module.id()).unwrap();
                    for bits in [
                        None,
                        Some(-128),
                        Some(-17),
                        Some(-1),
                        Some(0),
                        Some(1),
                        Some(19),
                    ] {
                        let result = interp::run_function(
                            &context,
                            function,
                            vec![bits.map_or(Value::Poison, |n| {
                                Value::Int(APInt::new_signed(u64::from(width), n))
                            })],
                        );
                        let bits = if defined { Some(-128) } else { bits };
                        match bits.filter(|&n| {
                            divisor != 0
                                && !(signed && divisor == -1 && n == -128)
                                && !(signed && width == 1 && n % 2 != 0)
                        }) {
                            None => {
                                assert!(
                                    matches!(result, Err(interp::InterpError::Undefined(_))),
                                    "{operator}, i{width}, {divisor}: {result:?}"
                                );
                            }
                            Some(n) => {
                                let expected = if divisor == 1 {
                                    if remainder {
                                        0
                                    } else {
                                        n
                                    }
                                } else if signed {
                                    if remainder {
                                        n % divisor
                                    } else {
                                        n / divisor
                                    }
                                } else {
                                    let n = (n as u64) & 255;
                                    let d = (divisor as u64) & 255;
                                    if remainder {
                                        (n % d) as i64
                                    } else {
                                        (n / d) as i64
                                    }
                                };
                                let [Value::Int(value)] = result.as_ref().unwrap().as_slice()
                                else {
                                    panic!("{result:?}")
                                };
                                assert_eq!(
                                    value.clone().with_signed(false),
                                    APInt::new_signed(u64::from(width), expected)
                                        .with_signed(false)
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn division_by_one_publication_preserves_roots_and_metadata() {
    for control in ["root", "value", "block"] {
        let (context, module, function, ret, _) = parse(
            r#"module {
              %f = func.func @f(%x: !i8) -> !i8 {
                %one = constant {value = 1} : !i8
                %result = divui %one, %one : !i8
                func.return %result
              }
              module_end
            }"#,
        );
        let body = context.get_region(context.get_op(function).regions()[0]);
        let block = body.iter(context.clone()).next().unwrap();
        let operation = block.op_ids()[1];
        let reference = context.get_op(ret).value_operands()[0];
        let attribute = tir::attributes::AttributeValue::Array(
            vec![tir::attributes::AttributeValue::Value(reference)].into(),
        );
        if control == "value" {
            let mut attrs = context.get_op(function).attributes();
            attrs.push(context.named_attribute("hidden", attribute.clone()));
            context.set_op_attributes(function, attrs);
        } else if control == "block" {
            block.set_attr("hidden", attribute.clone());
        }
        let root = if control == "root" {
            operation
        } else {
            function
        };
        EqSatPass::new()
            .run(root, &context, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&context, module.id()).unwrap();
        assert!(
            context.has_operation(operation),
            "covered result must stay addressable"
        );
        if control == "value" {
            assert_eq!(context.get_op(function).attr("hidden"), Some(attribute));
            assert!(context.get_value(reference).defining_op().is_some());
        } else if control == "block" {
            assert_eq!(block.attr("hidden"), Some(attribute));
            assert!(context.get_value(reference).defining_op().is_some());
        }
        assert_eq!(execute(&context, function, 8, 0), APInt::new(8, 1));
    }
}

#[test]
fn complementary_comparisons_do_not_export_an_erased_constant_representative() {
    let source = include_str!("../../../../core/checks/Inputs/eqsat/cmp_complement.tir")
        .replace("%r = scf.switch", "%r, %unused = scf.switch")
        .replace("-> %x", "%probe_one = constant {value = 1} : !i1\n      %probe_zero = xori %probe_one, %probe_one : !i1\n      -> %x, %probe_zero")
        .replace("%y = xori %t, %k : !i1", "%probe_mask = constant {value = 1} : !i1\n      %y = xori %t, %probe_mask : !i1")
        .replace("-> %y", "%probe_zero = xori %y, %y : !i1\n      -> %y, %probe_zero")
        .replace("-> %r", "%probe_result = xori %r, %unused : !i1\n    -> %probe_result");
    let context = Context::with_default_dialects();
    let module = parse_operation(&context, &source, &MODULE).unwrap();
    let function = context
        .get_region(module.regions()[0])
        .op_ids()
        .into_iter()
        .find(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        .unwrap();
    let args = vec![
        Value::Int(APInt::new(32, 0)),
        Value::Int(APInt::new(32, 1)),
        Value::Int(APInt::new(1, 1)),
        Value::Int(APInt::new(1, 0)),
    ];
    for round in 0..2 {
        if round > 0 {
            EqSatPass::new()
                .run(function, &context, &AnalysisManager::new())
                .unwrap();
        }
        let values = interp::run_function(&context, function, args.clone()).unwrap();
        let [Value::Int(value)] = values.as_slice() else {
            panic!("expected integer");
        };
        assert_eq!(value.clone().with_signed(false), APInt::new(1, 1));
        tir::verify_op_tree(&context, module.id()).unwrap();
    }
}

#[test]
fn carried_result_forwarding_preserves_evaluation_in_graph_regions() {
    let source = include_str!("../../../../core/checks/Inputs/eqsat/theta_carried_port.tir")
        .replace("func.return %8", "-> %3");
    let context = Context::with_default_dialects();
    let module = parse_operation(&context, &source, &MODULE).unwrap();
    let function = context.get_region(module.regions()[0]).op_ids()[0];
    let mut pass = EqSatPass::new();
    for round in 0..2 {
        if round > 0 {
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
        }
        tir::verify_op_tree(&context, module.id()).unwrap();
        let args = vec![
            Value::Int(APInt::new(64, 0)),
            Value::Int(APInt::new(64, 1)),
            Value::Int(APInt::new(64, 1)),
        ];
        let values = interp::run_function(&context, function, args.clone()).unwrap();
        let [Value::Int(value)] = values.as_slice() else {
            panic!("expected integer");
        };
        assert_eq!(value.with_signed(false), APInt::new(32, 7));
        let mut poisoned = args;
        poisoned[1] = Value::Poison;
        assert!(
            interp::run_function(&context, function, poisoned).is_err(),
            "loop bounds must still be evaluated, round {round}"
        );
    }
}

#[test]
fn cfg_join_parameters_forward_only_uniform_incoming_values() {
    for (source, uniform) in [
        (
            include_str!("../../../../core/checks/Inputs/eqsat/join_argument_constant.tir"),
            true,
        ),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/join_argument_conflicting.tir"),
            false,
        ),
    ] {
        let (context, module, function, _, _) = parse(source);
        let body = context.get_region(context.get_op(function).regions()[0]);
        let blocks = body.iter(context.clone()).collect::<Vec<_>>();
        let literal = context.get_op(blocks[0].op_ids()[0]).results()[0];
        let join = blocks.last().unwrap();
        let parameter = join.arguments()[0].id();
        let ret = *join.op_ids().last().unwrap();
        let sum = (!uniform).then(|| join.op_ids()[1]);
        let mut pass = EqSatPass::new();
        for round in 0..=2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&context, module.id()).unwrap();
                if let Some(sum) = sum {
                    assert_eq!(context.get_op(sum).operands()[0], parameter);
                    assert_eq!(
                        context.get_op(ret).operands().as_slice(),
                        context.get_op(sum).results().as_slice()
                    );
                } else {
                    assert_eq!(
                        context.get_op(ret).operands().as_slice(),
                        [literal],
                        "uniform join reaches the return"
                    );
                }
            }
            for (c, expected) in [
                (0, if uniform { 5 } else { 3 }),
                (1, if uniform { 5 } else { 2 }),
            ] {
                assert_eq!(execute(&context, function, 1, c), APInt::new(32, expected));
            }
            assert!(matches!(
                interp::run_function(&context, function, vec![Value::Poison]),
                Err(interp::InterpError::Undefined(_))
            ));
        }
    }
}

#[test]
fn cfg_join_parameters_preserve_unproved_values_and_unavailable_witnesses() {
    let unknown = include_str!("../../../../core/checks/Inputs/eqsat/join_argument_constant.tir")
        .replace("%c: !i1", "%c: !i1, %x: !i32")
        .replace("cfg.br ^bb3(%2 : !i32)", "cfg.br ^bb3(%x : !i32)");
    let nested = unknown.replace(
        "cfg.br ^bb3(%1 : !i32)",
        r#"%p = cmpi %0, %x {predicate = "eq"} : !i1
    %unused = scf.switch %p { -> %0 } { -> %x }
    cfg.br ^bb3(%1 : !i32)"#,
    );
    let retained = include_str!("../../../../core/checks/Inputs/eqsat/join_argument_constant.tir")
        .replace("%c: !i1", "%c: !i1, %x: !i32")
        .replace("%0 = constant {value = 5} : !i32", "%one = constant {value = 1} : !i32\n    %unused = remui %one, %one : !i32\n    %0 = constant {value = 1} : !i32");
    let implicit = r#"module {
  %f = func.func @f(%c: !i1, %x: !i32) -> !i32 {
    %false = constant {value = 0} : !i1
    %five = constant {value = 5} : !i32
    %zero = constant {value = 0} : !i32
    cfg.cond_br %c, ^bb0(%false, %five : !i1, !i32), ^exit
  ^exit:
    %product = muli %x, %zero : !i32
    func.return %product
  }
  module_end
}"#;
    let backedge = r#"module {
  %f = func.func @f(%c: !i1, %x: !i32) -> !i32 {
    %five = constant {value = 5} : !i32
    %one = constant {value = 1} : !i32
    %false = constant {value = 0} : !i1
    cfg.br ^header(%five, %c : !i32, !i1)
  ^header(%v: !i32, %i: !i1):
    cfg.cond_br %i, ^body, ^exit(%v : !i32)
  ^body:
    %next = addi %v, %one : !i32
    cfg.br ^header(%next, %false : !i32, !i1)
  ^exit(%out: !i32):
    func.return %out
  }
  module_end
}"#;
    let unavailable = r#"module {
  %f = func.func @f(%c: !i1, %x: !i32) -> !i32 {
    cfg.cond_br %c, ^left, ^right
  ^left:
    %a = constant {value = 5} : !i32
    cfg.br ^left_arg(%a : !i32)
  ^right:
    %b = constant {value = 5} : !i32
    cfg.br ^right_arg(%b : !i32)
  ^left_arg(%l: !i32):
    cfg.br ^exit(%l : !i32)
  ^right_arg(%r: !i32):
    cfg.br ^exit(%r : !i32)
  ^exit(%out: !i32):
    func.return %out
  }
  module_end
}"#;
    for (source, fixed_false, when_true) in [
        (unknown.as_str(), None, 5),
        (nested.as_str(), None, 5),
        (implicit, Some(0), 0),
        (backedge, Some(5), 6),
        (unavailable, Some(5), 5),
        (retained.as_str(), Some(1), 1),
    ] {
        let (context, module, function, _, _) = parse(source);
        let mut pass = EqSatPass::new();
        for round in 0..=2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&context, module.id()).unwrap();
            }
            if source == implicit {
                let result = interp::run_function(
                    &context,
                    function,
                    vec![Value::Int(APInt::new(1, 0)), Value::Poison],
                )
                .unwrap();
                assert!(
                    matches!(result.as_slice(), [Value::Poison]),
                    "entry input remains potentially poison"
                );
            }
            for x in [9, 17] {
                for (c, expected) in [(0, fixed_false.unwrap_or(x)), (1, when_true)] {
                    let result = interp::run_function(
                        &context,
                        function,
                        vec![
                            Value::Int(APInt::new(1, c)),
                            Value::Int(APInt::new_signed(32, x)),
                        ],
                    )
                    .unwrap();
                    let [Value::Int(actual)] = result.as_slice() else {
                        panic!("expected integer")
                    };
                    assert_eq!(
                        actual.with_signed(false),
                        APInt::new_signed(32, expected).with_signed(false)
                    );
                }
                assert!(matches!(
                    interp::run_function(
                        &context,
                        function,
                        vec![Value::Poison, Value::Int(APInt::new_signed(32, x))]
                    ),
                    Err(interp::InterpError::Undefined(_))
                ));
            }
        }
    }
}

#[test]
fn unused_truncations_are_removed_without_losing_used_values() {
    let original = include_str!("../../../../core/checks/Inputs/eqsat/cse.tir");
    for mode in ["unused", "both", "metadata"] {
        let source = if mode == "both" {
            original.replace(
                "func.return %0",
                "%sum = addi %0, %1 : !i16\n    func.return %sum",
            )
        } else {
            original.to_owned()
        };
        let (context, module, function, _, _) = parse(&source);
        let body = context.get_region(context.get_op(function).regions()[0]);
        if mode == "metadata" {
            let value = context.get_op(body.op_ids()[1]).value_results()[0];
            let mut attrs = context.get_op(function).attributes();
            attrs.push(
                context.named_attribute("observed", tir::attributes::AttributeValue::Value(value)),
            );
            context.set_op_attributes(function, attrs);
        }
        for round in 0..2 {
            if round > 0 {
                EqSatPass::new()
                    .run(function, &context, &AnalysisManager::new())
                    .unwrap();
                if mode == "unused" {
                    assert_eq!(
                        body.op_ids()
                            .into_iter()
                            .filter(|&id| context.get_op(id).is_operation(&tir::builtin::TRUNCI))
                            .count(),
                        1
                    );
                }
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            if mode == "metadata" {
                let Some(tir::attributes::AttributeValue::Value(value)) =
                    context.get_op(function).attr("observed")
                else {
                    panic!("expected value reference");
                };
                assert!(context.has_operation(context.get_value(value).defining_op().unwrap()));
            }
            let factor = if mode == "both" { 2 } else { 1 };
            for input in [0i64, 0x12345678, -1] {
                assert_eq!(
                    execute(&context, function, 32, input),
                    APInt::new(16, ((input as u64 & 0xffff) * factor) & 0xffff)
                );
            }
            assert!(matches!(
                interp::run_function(&context, function, vec![Value::Poison])
                    .unwrap()
                    .as_slice(),
                [Value::Poison]
            ));
        }
    }
}

mod fp_motion;

mod cfg_contexts;

mod static_fold;

mod scalar_carriers;
