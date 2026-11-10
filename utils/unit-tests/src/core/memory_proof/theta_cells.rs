use super::*;
use tir::{AnalysisManager, Pass};

const CELLS: [&str; 4] = [
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_while_iter_arg.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_condition_latched.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_ports_cross.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_condition_latched_cross.tir"),
];

#[test]
fn theta_private_cells_preserve_values_poison_and_controller_demand() {
    for (shape, original) in CELLS.iter().enumerate() {
        for width in [8, 16, 32, 64] {
            let source = original.replace("!i32", &format!("!i{width}")).replace(
                "size = 4, align = 4",
                &format!("size = {}, align = {}", width / 8, width / 8),
            );
            let (c, f, _, _) = fixture(&source);
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            let mut state = ExecutionState::default();
            let external = state.memory.alloc(8, 1);
            state.memory.write(external, &[0xa5; 8]).unwrap();
            for round in 0..=2 {
                if round > 0 {
                    tir::passes::EqSatPass::new()
                        .run(f, &c, &AnalysisManager::new())
                        .unwrap();
                    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
                    let output = printed_memory(&c, f);
                    assert!(!output.contains("ptr."), "shape={shape} i{width}\n{output}");
                    if width == 32 {
                        filecheck::verify(
                            &filecheck::Source::new("theta cell", *original),
                            &filecheck::Source::new("optimized", &output),
                            &filecheck::Config::default(),
                        )
                        .unwrap_or_else(|error| panic!("{error}\n{output}"));
                    }
                }
                for seed in [0, 1, u64::MAX, 1 << (width - 1)] {
                    for value in [Value::Int(APInt::new(width, seed)), Value::Poison] {
                        if shape == 3 && (seed != 0 || matches!(value, Value::Poison)) {
                            continue;
                        }
                        let mut arguments = vec![Value::Int(APInt::new(1, 0))];
                        if shape != 3 {
                            arguments.push(value.clone());
                        }
                        if shape == 2 {
                            arguments.push(Value::Int(APInt::new(width, 29)));
                        }
                        let actual =
                            interp::run_function_in_state(&c, f, arguments, &mut state, Some(512))
                                .unwrap();
                        match (&actual[..], &value) {
                            ([Value::Int(actual)], Value::Int(expected)) => {
                                assert_eq!(actual.with_signed(false), expected.with_signed(false))
                            }
                            ([Value::Poison], Value::Poison) => {}
                            _ => panic!("shape={shape} i{width} round={round}: {actual:?}"),
                        }
                    }
                }
                let mut arguments = vec![Value::Poison];
                if shape != 3 {
                    arguments.push(Value::Int(APInt::new(width, 7)));
                }
                if shape == 2 {
                    arguments.push(Value::Int(APInt::new(width, 29)));
                }
                let poisoned =
                    interp::run_function_in_state(&c, f, arguments, &mut state, Some(512));
                assert!(
                    matches!(poisoned, Err(interp::InterpError::Undefined(_))),
                    "{poisoned:?}"
                );
                let mut arguments = vec![Value::Int(APInt::new(1, 1))];
                if shape != 3 {
                    arguments.push(Value::Int(APInt::new(width, 7)));
                }
                if shape == 2 {
                    arguments.push(Value::Int(APInt::new(width, 29)));
                }
                let running =
                    interp::run_function_in_state(&c, f, arguments, &mut state, Some(512));
                assert!(
                    matches!(running, Err(interp::InterpError::StepLimit)),
                    "{running:?}"
                );
                assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
            }
        }
    }
}

#[test]
fn theta_private_cells_carry_each_finite_iteration() {
    for (shape, source) in [
            CELLS[0].replace(
                "      %7, %8 = scf.switch %0 args(%6)",
                "      %current, %read = ptr.load %2 state(%6) : !i32\n      %limit = constant {value = 8} : !i32\n      %more = cmpi %current, %limit {predicate = \"ult\"} : !i1\n      %7, %8 = scf.switch %more args(%read)",
            ),
            include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_switch_arms.tir").replace(
                "      %8, %9 = scf.switch %0 args(%7)",
                "      %current, %read = ptr.load %3 state(%7) : !i32\n      %limit = constant {value = 8} : !i32\n      %more = cmpi %current, %limit {predicate = \"ult\"} : !i1\n      %8, %9 = scf.switch %more args(%read)",
            ),
            CELLS[2].replace(
                "%10, %11 = scf.switch %0 args(%9)",
                "%15, %16 = ptr.load %3 state(%9) : !i32\n      %17, %18 = ptr.load %4 state(%16) : !i32\n      %condition = cmpi %15, %1 {predicate = \"eq\"} : !i1\n      %10, %11 = scf.switch %condition args(%18)",
            ).replace(
                "        %15, %16 = ptr.load %3 state(%14) : !i32\n        %17, %18 = ptr.load %4 state(%16) : !i32\n        %19 = ptr.store %17, %3 state(%18)",
                "        %19 = ptr.store %17, %3 state(%14)",
            ),
        finite_theta_sum(CELLS[3]),
    ].into_iter().enumerate() {
        let (c, f, _, _) = fixture(&source);
        for round in 0..=2 {
            if round > 0 {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
                assert!(!printed_memory(&c, f).contains("ptr."));
            }
            let cases: &[(u32, u32)] = if shape == 3 {
                &[(0, 0), (1, 0), (2, 1), (5, 10), (8, 28)]
            } else {
                &[
                (1, 8),
                (2, 8),
                (4, 8),
                (7, 14),
                (8, 8),
                (11, 11),
                (16, 16),
                (u32::MAX, u32::MAX),
                ]
            };
            for &(seed, expected) in cases {
                let mut arguments = vec![Value::Int(APInt::new(1, 0))];
                if shape == 1 {
                    arguments.push(Value::Int(APInt::new(32, 1)));
                }
                arguments.push(Value::Int(APInt::new(32, seed as u64)));
                if shape == 2 {
                    arguments.push(Value::Int(APInt::new(32, 29)));
                }
                let expected = if shape == 2 { 29 } else { expected };
                let actual = interp::run_function_within(&c, f, arguments, Some(512)).unwrap();
                assert!(
                    matches!(&actual[..], [Value::Int(value)] if value.to_u64() == expected as u64),
                    "seed={seed} round={round}: {actual:?}"
                );
            }
        }
    }
}

#[test]
fn theta_private_cells_keep_unsupported_and_undemanded_memory() {
    let source = CELLS[0];
    for source in [
        source.replace("size = 4", "size = 1"),
        source.replace("state(%13)", "state(%13) {volatile = true}"),
        source.replace("state(%5) :", "state(%5) {unknown_contract = true} :"),
        source.replace("    %4 = ptr.store %1, %2 state(%3)\n", "").replace("%6 = %4", "%6 = %3"),
        source.replace(
            "    %17, %18 = ptr.load",
            "    %escape = ptr.cmp %2, %2 {predicate = \"eq\"} : !i1\n    %17, %18 = ptr.load",
        ),
        source.replace(
            "module {",
            "module {layout.spec = #layout.spec<endianness = \"big\", !ptr.p = {size = 64, abi = 64}>} {",
        ),
        source.replace("-> %17, %18", "-> %1, %18"),
        source.replace(
            "    %17, %18 = ptr.load %2 state(%5)",
            "    %reset = ptr.store %1, %2 state(%5)\n    %17, %18 = ptr.load %2 state(%reset)",
        ),
        source.replace(
            "    %17, %18 = ptr.load",
            "    %other = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>\n    ptr.store %1, %other\n    %17, %18 = ptr.load",
        ),
    ] {
        let (c, f, _, _) = fixture(&source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        let output = printed_memory(&c, f);
        assert!(output.contains("ptr.alloca"), "{source}\n{output}");
        assert!(output.contains("scf.loop"), "{source}\n{output}");
        if !source.contains("state(%reset)") {
            assert!(output.contains("ptr.load"), "{source}\n{output}");
        }
    }
    for selected in 0..4 {
        let (c, f, _, _) = fixture(source);
        let ops = c.get_region(c.get_op(f).regions()[0]).op_ids();
        let operation = ops
            .iter()
            .find(|&&id| {
                c.get_op(id).is_operation(match selected {
                    0 => &tir::ptr::ALLOCA,
                    1 | 2 => &tir::ptr::LOAD,
                    _ => &tir::scf::LOOP,
                })
            })
            .unwrap();
        let value = c.get_op(*operation).results()[usize::from(selected == 1)];
        let mut attributes = c.get_op(f).attributes();
        attributes.push(c.named_attribute(
            "keep",
            tir::attributes::AttributeValue::Array(
                vec![tir::attributes::AttributeValue::Value(value)].into(),
            ),
        ));
        c.set_op_attributes(f, attributes);
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        let output = printed_memory(&c, f);
        assert!(
            output.contains("ptr.alloca") && output.contains("ptr.load"),
            "{output}"
        );
        assert!(c.get_value(value).defining_op().is_some());
    }
}

#[test]
fn theta_private_cells_preserve_faults_and_unused_control_demand() {
    let source = CELLS[0];
    let foreign = source.replace(
        "%1: !i32) -> !i32",
        "%1: !i32, %foreign: !ptr.p<!i32>) -> !i32",
    ).replace(
        "    -> %17, %18",
        "    %fault, %fault_state = ptr.load %foreign state(%18) : !i32\n    -> %17, %fault_state",
    );
    let unused = source.replace("-> %17, %18", "-> %1, %18");
    let overwritten = source.replace(
        "    %17, %18 = ptr.load %2 state(%5)",
        "    %reset = ptr.store %1, %2 state(%5)\n    %17, %18 = ptr.load %2 state(%reset)",
    );
    for (case, source) in [
        source.replace("size = 4", "size = 1"),
        foreign,
        unused,
        overwritten,
    ]
    .iter()
    .enumerate()
    {
        let (c, f, _, _) = fixture(source);
        for round in 0..=1 {
            if round > 0 {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            }
            for selector in [Some(0), Some(1), None] {
                if case < 2 && selector != Some(0) {
                    continue;
                }
                let mut arguments = vec![
                    selector.map_or(Value::Poison, |bits| Value::Int(APInt::new(1, bits))),
                    Value::Int(APInt::new(32, 7)),
                ];
                if case == 1 {
                    arguments.push(Value::Ptr(0));
                }
                let actual = interp::run_function_within(&c, f, arguments, Some(512));
                match (case, selector) {
                    (0 | 1, _) => assert!(
                        matches!(actual, Err(interp::InterpError::OutOfBounds { .. })),
                        "case={case} round={round}: {actual:?}"
                    ),
                    (_, None) => assert!(
                        matches!(actual, Err(interp::InterpError::Undefined(_))),
                        "case={case} round={round}: {actual:?}"
                    ),
                    (_, Some(1)) => assert!(
                        matches!(actual, Err(interp::InterpError::StepLimit)),
                        "case={case} round={round}: {actual:?}"
                    ),
                    _ => assert!(
                        matches!(&actual.unwrap()[..], [Value::Int(value)] if value.to_u64() == 7)
                    ),
                }
            }
        }
    }
}

#[test]
fn theta_private_cells_preserve_overwritten_value_faults() {
    let source = CELLS[0].replace("        %15 = ptr.store %14, %2 state(%13)",
        "        %zero_divisor = constant {value = 0} : !i32\n        %bad = divsi %1, %zero_divisor : !i32\n        %discarded = ptr.store %bad, %2 state(%13)\n        %15 = ptr.store %14, %2 state(%discarded)");
    let (c, f, _, _) = fixture(&source);
    for round in 0..2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
        }
        let result = interp::run_function_within(
            &c,
            f,
            vec![Value::Int(APInt::new(1, 1)), Value::Int(APInt::new(32, 8))],
            Some(512),
        );
        assert!(
            matches!(result, Err(interp::InterpError::Undefined(_))),
            "{result:?}"
        );
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
    }
}

#[test]
fn theta_private_cell_with_accumulator_keeps_sums_and_memory_only_control() {
    let original =
        include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_invariant_slot_while.tir");
    let finite = original.replace(
        "%10, %11, %12 = scf.switch %0",
        "%bound = constant {value = 20} : !i32\n      %more = cmpi %8, %bound {predicate = \"ult\"} : !i1\n      %10, %11, %12 = scf.switch %more",
    );
    let (c, f, _, _) = fixture(&finite);
    for round in 0..=2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            assert!(!printed_memory(&c, f).contains("ptr."));
        }
        for (seed, initial, expected) in [
            (1, 0, 20),
            (2, 1, 21),
            (3, 0, 21),
            (3, 19, 22),
            (7, 0, 21),
            (7, 1, 22),
            (7, 20, 20),
            (7, 27, 27),
        ] {
            let actual = interp::run_function_within(
                &c,
                f,
                vec![
                    Value::Poison,
                    Value::Int(APInt::new(32, seed)),
                    Value::Int(APInt::new(32, initial)),
                ],
                Some(512),
            )
            .unwrap();
            assert!(matches!(&actual[..], [Value::Int(value)] if value.to_u64() == expected));
        }
    }

    let source = r#"module {
      %fn = func.func @independent(%selector: !i1, %seed: !i32, %initial: !i32, %continue: !i1) -> !i32 {
        %cell = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
        %entry = state.entry_state : !state<memory>
        %stored = ptr.store %seed, %cell state(%entry)
        %result, %final = scf.loop (%acc = %initial, %state = %stored) {
          %joined = scf.switch %selector args(%state) (%left) { -> %left }
          (%right) { -> %right }
          -> %continue, %acc, %acc, %joined, %joined
        }
        -> %result, %final
      }
      module_end
    }"#;
    let (c, f, _, _) = fixture(source);
    for round in 0..=2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            let output = printed_memory(&c, f);
            assert!(!output.contains("ptr."));
            assert!(output.contains("scf.switch"));
        }
        for control in [0, 1] {
            let actual = interp::run_function_within(
                &c,
                f,
                vec![
                    Value::Poison,
                    Value::Int(APInt::new(32, 7)),
                    Value::Int(APInt::new(32, 2)),
                    Value::Int(APInt::new(1, control)),
                ],
                Some(512),
            );
            assert!(
                matches!(actual, Err(interp::InterpError::Undefined(_))),
                "{actual:?}"
            );
        }
    }
}

#[test]
fn theta_private_cell_pairs_preserve_group_guards_and_partial_evaluation() {
    let source = CELLS[2];
    let cases = [
        (source.replace("%5 = state.entry_state", "%escape = ptr.cmp %4, %4 {predicate = \"eq\"} : !i1\n    %5 = state.entry_state"), false),
        (source.replace("ptr.store %15, %4 state(%19)", "ptr.store %15, %4 state(%19) {volatile = true}"), false),
        (source.replace("%6 = ptr.store %1,", "%bad = divsi %1, %1 : !i32\n    %6 = ptr.store %bad,"), false),
        (source.replace("%7 = ptr.store %2,", "%bad = divsi %2, %2 : !i32\n    %7 = ptr.store %bad,"), false),
        (source.replace("%5 = state.entry_state", "%foreign = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>\n    %5 = state.entry_state"), false),
        (source.replace("size = 4, align = 4", "size = 2, align = 2"), false),
        (source.replace("-> %22, %23", "-> %1, %23"), false),
        (source.replace("ptr.store %15, %4 state(%19)", "ptr.store %15, %4 state(%19) {unknown_contract = true}"), false),
        (source.replace("%19 = ptr.store %17,", "%bad = divsi %17, %1 : !i32\n        %19 = ptr.store %bad,"), true),
        (source.replace("%10, %11 = scf.switch %0", "%zero = constant {value = 0} : !i1\n      %bad = divsi %0, %zero : !i1\n      %10, %11 = scf.switch %bad"), true),
        (source.replace("%5 = state.entry_state", "%fp_entry = state.entry_state : !state<fp.env>\n    %flags = constant {value = 1} : !i5\n    %fp_next = fp.raise_flags %flags state(%fp_entry) {trapping = false}\n    %5 = state.entry_state").replace("-> %22, %23", "-> %22, %23, %fp_next"), false),
        (source.replace("module {", "module {layout.spec = #layout.spec<endianness = \"big\", !ptr.p = {size = 64, abi = 64}>} {"), false),
        (source.replace("    %7 = ptr.store %2, %4 state(%6)\n", "").replace("%9 = %7", "%9 = %6"), false),
        (source.replace("%5 = state.entry_state", "%third = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>\n    %5 = state.entry_state").replace("%8 = scf.loop (%9 = %7)", "%s3 = ptr.store %1, %third state(%7)\n    %third_value, %s4 = ptr.load %third state(%s3) : !i32\n    %8 = scf.loop (%9 = %s4)"), false),
    ];
    let sum = CELLS[3];
    let finite = finite_theta_sum(sum);
    let sum_cases = [
        (sum.to_owned(), true),
        (finite.clone(), true),
        (finite.replace("addi %14, %16 :", "addi %14, %16 {nsw = true} :"), true),
        (finite.replace("%18 = addi %14, %16 :", "%18 = divsi %14, %16 :"), true),
        (finite.replace("%condition = cmpi %16, %limit {predicate = \"ult\"} : !i1", "%zero = constant {value = 0} : !i32\n      %bad = divsi %limit, %zero : !i32\n      %condition = cmpi %16, %bad {predicate = \"ult\"} : !i1"), true),
        (sum.replace("ptr.store %21, %2 state(%19)", "ptr.store %21, %2 state(%19) {unknown_contract = true}"), false),
        (sum.replace("ptr.store %21, %2 state(%19)", "ptr.store %21, %2 state(%19) {volatile = true}"), false),
        (sum.replace("%5 = ptr.store %3,", "%bad = divsi %3, %3 : !i32\n    %5 = ptr.store %bad,"), false),
        (sum.replace("%4 = state.entry_state", "%escape = ptr.cmp %2, %2 {predicate = \"eq\"} : !i1\n    %4 = state.entry_state"), false),
        (finite.replace("!i32", "!i8").replace("size = 4, align = 4", "size = 1, align = 1"), true),
        (finite.replace("!i32", "!i16").replace("size = 4, align = 4", "size = 2, align = 2"), true),
        (finite.replace("!i32", "!i64").replace("size = 4, align = 4", "size = 8, align = 8"), true),
    ];
    for (source, promote) in cases.into_iter().chain(sum_cases) {
        theta_pair_observations(&source, promote);
    }
}

fn theta_pair_observations(source: &str, promote: bool) {
    let (c, f, _, _) = fixture(source);
    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
    let types = c
        .get_region(c.get_op(f).regions()[0])
        .ports()
        .iter()
        .map(tir::Value::ty)
        .collect::<Vec<_>>();
    let observe = || {
        let mut observations = Vec::new();
        for selector in [Some(0), Some(1), None] {
            for (left, right) in [(11, 29), (0, 7), (9, 0)] {
                for poisoned in 0..types.len() {
                    let mut state = ExecutionState::default();
                    state.fp_environment.flags = 0x10;
                    let external = state.memory.alloc(8, 1);
                    state.memory.write(external, &[0xa5; 8]).unwrap();
                    let mut args = vec![
                        selector.map_or(Value::Poison, |bits| Value::Int(APInt::new(1, bits)))
                    ];
                    args.extend([left, right].into_iter().zip(types.iter().skip(1)).map(
                        |(bits, &ty)| {
                            Value::Int(APInt::new(
                                tir::builtin::IntegerType::view(&c, ty).unwrap().width(),
                                bits,
                            ))
                        },
                    ));
                    if poisoned > 0 {
                        args[poisoned] = Value::Poison;
                    }
                    let result =
                        match interp::run_function_in_state(&c, f, args, &mut state, Some(512)) {
                            Ok(values) => format!(
                                "{:?}",
                                values
                                    .into_iter()
                                    .map(|v| match v {
                                        Value::Int(bits) => Value::Int(bits.with_signed(false)),
                                        other => other,
                                    })
                                    .collect::<Vec<_>>()
                            ),
                            Err(interp::InterpError::Undefined(_)) => "Undefined".to_owned(),
                            Err(error) => format!("{error:?}"),
                        };
                    assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
                    if source.contains("fp.raise_flags") && selector == Some(0) && poisoned == 0 {
                        assert_eq!(state.fp_environment.flags, 0x11);
                    }
                    observations.push((result, state.fp_environment));
                }
            }
        }
        observations
    };
    let before = observe();
    for _ in 0..2 {
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        let output = printed_memory(&c, f);
        if promote {
            assert!(!output.contains("ptr."), "{source}\n{output}");
        } else {
            assert_eq!(
                output.matches("ptr.alloca").count(),
                source.matches("ptr.alloca").count(),
                "{source}\n{output}"
            );
            assert!(output.matches("ptr.load").count() >= 3, "{output}");
            assert_eq!(
                output.matches("ptr.store").count(),
                source.matches("ptr.store").count(),
                "{output}"
            );
            for marker in ["volatile = true", "unknown_contract = true"] {
                if source.contains(marker) {
                    assert!(output.contains(marker), "{output}");
                }
            }
        }
        assert_eq!(observe(), before, "{source}");
    }
}

fn finite_theta_sum(source: &str) -> String {
    source.replace(
            "@sum_to(%0: !i1)",
            "@sum_to(%0: !i1, %limit: !i32)",
        ).replace(
            "%9, %10 = scf.switch %0 args(%8)",
            "%14, %15 = ptr.load %1 state(%8) : !i32\n      %16, %17 = ptr.load %2 state(%15) : !i32\n      %condition = cmpi %16, %limit {predicate = \"ult\"} : !i1\n      %9, %10 = scf.switch %condition args(%17)",
        ).replace(
            "        %14, %15 = ptr.load %1 state(%13) : !i32\n        %16, %17 = ptr.load %2 state(%15) : !i32\n",
            "",
        ).replace(
            "%19 = ptr.store %18, %1 state(%17)",
            "%19 = ptr.store %18, %1 state(%13)",
        )
}
