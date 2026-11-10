use super::*;

const PROVED: &str =
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_hypothesis_proved.tir");
const FOLDED: &str =
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_hypothesis_folded_init.tir");

const NESTED: &str =
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_hypothesis_nested.tir");

fn integer(width: u64, bits: u64) -> Value {
    Value::Int(APInt::new(width, bits))
}

#[test]
fn gamma_constant_materialization_preserves_wide_integer_bits() {
    for (literal, base, increment, sum) in [
        ("18446744073709551616", [0, 1], 2, [2, 1]),
        (
            "9223372036854775807",
            [i64::MAX as u64, 0],
            1,
            [1_u64 << 63, 0],
        ),
    ] {
        let context = Context::with_default_dialects();
        let source = format!(
            r#"module {{
  %f = func.func @f(%c: !i1) -> !i128 {{
    %a = constant {{value = {literal}}} : !i128
    %b = constant {{value = {increment}}} : !i128
    %r = scf.switch %c {{
      %x = addi %a, %b : !i128
      -> %x
    }} {{
      -> %a
    }}
    -> %r
  }}
  module_end
}}"#
        );
        let module = parse_operation(&context, &source, &MODULE).unwrap();
        let function = context.get_region(module.regions()[0]).op_ids()[0];
        for optimized in [false, true] {
            if optimized {
                EqSatPass::new()
                    .run(function, &context, &AnalysisManager::new())
                    .unwrap();
                assert!(!printed_ir(&context, &module).contains("addi "));
            }
            tir::verify_op_tree(&context, module.id()).unwrap();
            for (selector, expected) in [(0, sum), (1, base)] {
                let actual =
                    interp::run_function(&context, function, vec![integer(1, selector)]).unwrap();
                let [Value::Int(value)] = actual.as_slice() else {
                    panic!("expected integer result")
                };
                assert_eq!(
                    value.with_signed(false),
                    APInt::from_limbs(128, &expected),
                    "{literal}, optimized={optimized}, selector={selector}"
                );
            }
        }
    }
}

fn assert_carrier_controls(
    context: &Context,
    function: tir::OpId,
    carried: Option<&Value>,
    constant: u64,
) {
    let run = |args| {
        let mut state = interp::ExecutionState::default();
        let external = state.memory.alloc(8, 1);
        state.memory.write(external, &[0xa5; 8]).unwrap();
        let result = interp::run_function_in_state(context, function, args, &mut state, Some(512));
        assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
        result
    };
    for bounds in [[0, 0, 1], [0, 3, 1], [5, 2, 1], [0, 9, 2]] {
        let mut args: Vec<_> = bounds.into_iter().map(|n| integer(64, n)).collect();
        args.extend(carried.cloned());
        let result = run(args).unwrap();
        let expected = carried.cloned().unwrap_or_else(|| integer(32, constant));
        match (result.as_slice(), expected) {
            ([Value::Int(actual)], Value::Int(expected)) => {
                assert_eq!(actual.with_signed(false), expected.with_signed(false));
            }
            ([Value::Poison], Value::Poison) => {}
            _ => panic!("wrong carried result {result:?}"),
        }
    }
    for poisoned in 0..3 {
        let mut args = vec![integer(64, 0), integer(64, 3), integer(64, 1)];
        args[poisoned] = Value::Poison;
        args.extend(carried.cloned());
        let result = run(args);
        assert!(
            matches!(&result, Err(interp::InterpError::Message(message)) if message == "scf.for bounds must be integers"),
            "{result:?}"
        );
    }
    for bounds in [[0, 3, 0], [0, 100000, 1]] {
        let mut args: Vec<_> = bounds.into_iter().map(|n| integer(64, n)).collect();
        args.extend(carried.cloned());
        assert!(matches!(run(args), Err(interp::InterpError::StepLimit)));
    }
}

#[test]
fn zero_feedback_folds_scalars_and_preserves_carried_values_and_loop_control() {
    for (original, constant, loops) in [(PROVED, 7, 1), (FOLDED, 7, 1), (NESTED, 14, 2)] {
        let (context, module, function, _, _) = parse(original);
        assert_carrier_controls(&context, function, None, constant);
        let mut pass = EqSatPass::new();
        for _ in 0..2 {
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            let output = printed_ir(&context, &module);
            filecheck::verify(
                &filecheck::Source::new("zero feedback", original),
                &filecheck::Source::new("optimized", &output),
                &filecheck::Config::default(),
            )
            .unwrap_or_else(|error| panic!("{error}\n{output}"));
            assert!(!output.contains("addi"), "{output}");
            assert_eq!(output.matches("scf.ordered_for").count(), loops, "{output}");
            assert_carrier_controls(&context, function, None, constant);
        }
    }
    let source = PROVED
        .replace("%2: !i64)", "%2: !i64, %seed: !i32)")
        .replace("    %3 = constant {value = 7} : !i32\n", "")
        .replace("%3", "%seed");
    let (context, module, function, _, _) = parse(&source);
    let mut pass = EqSatPass::new();
    for round in 0..=2 {
        if round > 0 {
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            let output = printed_ir(&context, &module);
            assert!(!output.contains("addi"), "{output}");
            assert_eq!(output.matches("scf.ordered_for").count(), 1, "{output}");
        }
        for carried in [integer(32, 0), integer(32, u32::MAX.into()), Value::Poison] {
            assert_carrier_controls(&context, function, Some(&carried), 7);
        }
    }
}

#[test]
fn changing_feedback_and_partial_or_metadata_producers_remain() {
    for (original, constant, loops, changing_values) in [
        (PROVED, 7, 1, [7, 10, 7, 12]),
        (NESTED, 14, 2, [14, 23, 14, 39]),
    ] {
        let changing = original.replace("value = 0", "value = 1");
        let (context, module, function, _, _) = parse(&changing);
        let args = || vec![integer(64, 0), integer(64, 3), integer(64, 1)];
        let mut pass = EqSatPass::new();
        for round in 0..=2 {
            if round > 0 {
                pass.run(function, &context, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&context, module.id()).unwrap();
                let output = printed_ir(&context, &module);
                assert!(output.contains("addi"), "{output}");
                assert_eq!(output.matches("scf.ordered_for").count(), loops, "{output}");
            }
            for (bounds, expected) in [[0, 0, 1], [0, 3, 1], [5, 2, 1], [0, 9, 2]]
                .into_iter()
                .zip(changing_values)
            {
                let result = interp::run_function_within(
                    &context,
                    function,
                    bounds.into_iter().map(|n| integer(64, n)).collect(),
                    Some(512),
                )
                .unwrap();
                assert!(
                    matches!(result.as_slice(), [Value::Int(value)] if value.to_u64() == expected),
                    "{result:?}"
                );
            }
        }
        for (operation, retained) in [
            ("divui %3, %4", "divui"),
            ("addi %3, %4 {tag = 1}", "tag = 1"),
            ("addi %3, %4 {nsw = true}", "nsw = true"),
        ] {
            let source = original.replace(
                "    %i1_end",
                &format!("    %unused = {operation} : !i32\n    %i1_end"),
            );
            let (context, module, function, _, _) = parse(&source);
            let mut pass = EqSatPass::new();
            for round in 0..=2 {
                if round > 0 {
                    pass.run(function, &context, &AnalysisManager::new())
                        .unwrap();
                    tir::verify_op_tree(&context, module.id()).unwrap();
                    let output = printed_ir(&context, &module);
                    assert!(output.contains(retained), "{output}");
                    assert_eq!(output.matches("scf.ordered_for").count(), loops, "{output}");
                }
                if retained == "divui" {
                    let result = interp::run_function_within(&context, function, args(), Some(512));
                    assert!(
                        matches!(result, Err(interp::InterpError::Undefined(_))),
                        "{result:?}"
                    );
                } else {
                    assert_carrier_controls(&context, function, None, constant);
                }
            }
        }
    }
    for (source, retained, capture) in [
        (
            NESTED.replace("value = 0}", "value = 0, marker = 73}"),
            "marker = 73",
            false,
        ),
        (
            NESTED.replace("addi %8, %4 :", "addi %8, %4 {tag = 74} :"),
            "tag = 74",
            false,
        ),
        (
            NESTED.replace("addi %8, %4 :", "addi %8, %4 {nsw = true} :"),
            "nsw = true",
            false,
        ),
        (NESTED.to_owned(), "", true),
    ] {
        let (context, module, function, _, _) = parse(&source);
        let capture = capture.then(|| {
            let body = context
                .get_region(context.get_op(function).regions()[0])
                .op_ids();
            let referenced = context.get_op(body[1]).results()[0];
            let owner = body[2];
            let attribute = tir::attributes::AttributeValue::Array(
                vec![tir::attributes::AttributeValue::Value(referenced)].into(),
            );
            let mut attributes = context.get_op(owner).attributes();
            attributes.push(context.named_attribute("capture", attribute.clone()));
            context.set_op_attributes(owner, attributes);
            (owner, referenced, attribute)
        });
        assert_carrier_controls(&context, function, None, 14);
        let mut pass = EqSatPass::new();
        for _ in 0..2 {
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            let output = printed_ir(&context, &module);
            assert_eq!(output.matches("scf.ordered_for").count(), 2, "{output}");
            if let Some((owner, referenced, attribute)) = &capture {
                assert!(context.get_value(*referenced).defining_op().is_some());
                assert_eq!(
                    context.get_op(*owner).attr("capture"),
                    Some(attribute.clone())
                );
            } else {
                assert!(output.contains(retained), "{output}");
            }
            assert_carrier_controls(&context, function, None, 14);
        }
    }
}

#[test]
fn graph_identity_exit_still_demands_its_controller() {
    let original =
        include_str!("../../../../../core/checks/Inputs/eqsat/theta_identity_forwarded_exit.tir");
    let source = original
        .replace("@main()", "@main(%limit: !i32)")
        .replace("      %7 = constant {value = 2} : !i32\n", "")
        .replace("%7", "%limit");
    let context = Context::with_default_dialects();
    let module = parse_operation(&context, &source, &MODULE).unwrap();
    tir::verify_op_tree(&context, module.id()).unwrap();
    let function = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()[0];
    let mut pass = EqSatPass::new();
    for round in 0..=2 {
        if round > 0 {
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            let output = printed_ir(&context, &module);
            assert_eq!(output.matches("scf.loop").count(), 1, "{output}");
        }
        let result =
            interp::run_function_within(&context, function, vec![integer(32, 2)], Some(512))
                .unwrap();
        assert!(
            matches!(result.as_slice(), [Value::Int(value)] if value.to_u64() == 49),
            "{result:?}"
        );
        let result =
            interp::run_function_within(&context, function, vec![Value::Poison], Some(512));
        assert!(
            matches!(result, Err(interp::InterpError::Undefined(_))),
            "{result:?}"
        );
    }
}

#[test]
fn gamma_scalar_arms_reuse_only_available_values_and_preserve_demand() {
    const ORIGINAL: &str = include_str!(
        "../../../../../core/checks/Inputs/eqsat/gamma_arm_reads_nothing_downstream.tir"
    );
    struct NoSpeculation;
    impl tir::Speculatable for NoSpeculation {
        fn is_speculatable(&self) -> bool {
            false
        }
    }
    for width in [8, 16, 32, 64] {
        for case in [
            "original",
            "ancestor-metadata",
            "constant-metadata",
            "operation-metadata",
            "sibling",
            "partial",
            "no-speculation",
            "root",
        ] {
            let source = ORIGINAL.replace("!i32", &format!("!i{width}"));
            let source = match case {
                "ancestor-metadata" => source.replace("value = 1}", "value = 1, marker = 73}"),
                "constant-metadata" => source.replace("value = -1}", "value = -1, marker = 74}"),
                "operation-metadata" => source.replace("subi %1, %4 :", "subi %1, %4 {unknown_contract = true} :"),
                "sibling" => format!(r#"module {{
                  %f = func.func @f(%c: !i1, %x: !i{width}) -> !i{width} {{
                    %r = scf.switch %c {{
                      %minus = constant {{value = -1}} : !i{width}
                      %v = subi %x, %minus : !i{width}
                      -> %v
                    }} {{
                      %one = constant {{value = 1}} : !i{width}
                      %v = addi %x, %one : !i{width}
                      -> %v
                    }}
                    -> %r
                  }}
                  module_end
                }}"#),
                "partial" => source.replace("%5 = subi %1, %4", &format!(
                    "%zero = constant {{value = 0}} : !i{width}\n      %quot = divsi %1, %zero : !i{width}\n      %5 = subi %quot, %4")),
                _ => source,
            };
            let context = Context::with_default_dialects();
            let module = parse_operation(&context, &source, &MODULE).unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            let function = context.get_region(module.regions()[0]).op_ids()[0];
            if case == "no-speculation" {
                context.register_op_interface::<dyn tir::Speculatable>(
                    "builtin",
                    "constant",
                    |_| tir::erase_op_interface::<dyn tir::Speculatable>(Box::new(NoSpeculation)),
                );
            }
            let body = context.get_region(context.get_op(function).regions()[0]);
            let gamma = body
                .op_ids()
                .into_iter()
                .find(|&id| context.get_op(id).regions().len() == 2)
                .unwrap();
            let arm = context.get_op(gamma).regions()[0];
            let mut state = interp::ExecutionState::default();
            let external = state.memory.alloc(8, 1);
            state.memory.write(external, &[0xa5; 8]).unwrap();
            let mut pass = EqSatPass::new();
            for round in 0..=2 {
                if round > 0 {
                    pass.run(
                        if case == "root" { gamma } else { function },
                        &context,
                        &AnalysisManager::new(),
                    )
                    .unwrap();
                    tir::verify_op_tree(&context, module.id()).unwrap();
                    let output = printed_ir(&context, &module);
                    if matches!(case, "original" | "ancestor-metadata") {
                        assert_eq!(
                            context.get_region(arm).op_ids().len(),
                            1,
                            "{case} i{width}\n{output}"
                        );
                        assert!(!output.contains("subi"), "{output}");
                        if case == "original" && width == 32 {
                            filecheck::verify(
                                &filecheck::Source::new("gamma ancestor", ORIGINAL),
                                &filecheck::Source::new("optimized", &output),
                                &filecheck::Config::default(),
                            )
                            .unwrap();
                        }
                    } else {
                        assert!(output.contains("subi"), "{case}\n{output}");
                    }
                    for (control, marker) in [
                        ("ancestor-metadata", "marker = 73"),
                        ("constant-metadata", "marker = 74"),
                        ("operation-metadata", "unknown_contract = true"),
                        ("partial", "divsi"),
                    ] {
                        if case == control {
                            assert!(output.contains(marker), "{output}");
                        }
                    }
                }
                assert_gamma_arm_results(&context, function, width, case, &mut state, external);
            }
        }
    }
}

fn assert_gamma_arm_results(
    context: &Context,
    function: tir::OpId,
    width: u64,
    case: &str,
    state: &mut interp::ExecutionState,
    external: u64,
) {
    let mask = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    for selector in [Some(0), Some(1), None] {
        for x in [0u64, 1, 7, mask, 1 << (width - 1)] {
            for poisoned in [false, true] {
                let actual = interp::run_function_in_state(
                    context,
                    function,
                    vec![
                        selector.map_or(Value::Poison, |bits| integer(1, bits)),
                        if poisoned {
                            Value::Poison
                        } else {
                            integer(width, x)
                        },
                    ],
                    state,
                    Some(512),
                );
                if selector.is_none() || (case == "partial" && selector == Some(0)) {
                    assert!(
                        matches!(actual, Err(interp::InterpError::Undefined(_))),
                        "{case}: {actual:?}"
                    );
                } else if poisoned {
                    assert!(matches!(actual.unwrap().as_slice(), [Value::Poison]));
                } else {
                    let expected = if case == "sibling" {
                        x.wrapping_add(1)
                    } else if selector == Some(0) {
                        x.wrapping_add(1).wrapping_mul(x.wrapping_add(1))
                    } else {
                        x.wrapping_add(3).wrapping_mul(x)
                    } & mask;
                    assert!(
                        matches!(actual.unwrap().as_slice(), [Value::Int(value)] if value.to_u64() == expected),
                        "{case} i{width} selector={selector:?} x={x}"
                    );
                }
                assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
            }
        }
    }
}
