use super::*;
use tir::{AnalysisManager, Pass};

const CELLS: [&str; 7] = [
    include_str!("../../../../../core/checks/Inputs/eqsat/gamma_load_one_arm.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/gamma_load_both_arms.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/gamma_load_switch_arms.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/gamma_load_nested.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/gamma_yield_from_before_the_gate.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/slot_read_in_gate_region.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/gamma_load_arms_cross.tir"),
];

#[test]
fn gamma_private_cells_preserve_selected_values_poison_and_controller_demand() {
    for (shape, original) in CELLS.iter().enumerate() {
        for width in [8, 16, 32, 64] {
            let source = original.replace("!i32", &format!("!i{width}")).replace(
                "size = 4, align = 4",
                &format!("size = {}, align = {}", width / 8, width / 8),
            );
            let (c, f, _, _) = fixture(&source);
            for round in 0..=1 {
                if round > 0 {
                    tir::passes::EqSatPass::new()
                        .run(f, &c, &AnalysisManager::new())
                        .unwrap();
                    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
                    let output = printed_memory(&c, f);
                    assert!(!output.contains("ptr."), "shape={shape} i{width}\n{output}");
                    if width == 32 {
                        filecheck::verify(
                            &filecheck::Source::new("gamma cell", *original),
                            &filecheck::Source::new("optimized", &output),
                            &filecheck::Config::default(),
                        )
                        .unwrap_or_else(|error| panic!("{error}\n{output}"));
                    }
                }
                let body = c.get_region(c.get_op(f).regions()[0]);
                let ports = body.ports();
                let cases: &[u64] = if shape == 2 {
                    &[0, 1, 2, 3, u32::MAX as u64]
                } else {
                    &[0, 1]
                };
                for &outer in cases {
                    for inner in 0..if shape == 3 { 2 } else { 1 } {
                        for seed in [0, 1, u64::MAX, 1 << (width - 1)] {
                            let mut arguments: Vec<_> = ports
                                .iter()
                                .enumerate()
                                .map(|(index, port)| {
                                    let bits = if index == 0 {
                                        outer
                                    } else if shape == 3 && index == 1 {
                                        inner
                                    } else {
                                        seed.wrapping_add(index as u64)
                                    };
                                    let bits_width = tir::builtin::IntegerType::view(&c, port.ty())
                                        .unwrap()
                                        .width();
                                    Value::Int(APInt::new(bits_width, bits))
                                })
                                .collect();
                            for poisoned in (0..arguments.len()).map(Some).chain([None]) {
                                let saved = poisoned.map(|index| arguments[index].clone());
                                if let Some(index) = poisoned {
                                    arguments[index] = Value::Poison;
                                }
                                let selected = match shape {
                                    0 => [1, 2][usize::from(outer != 0)],
                                    1 => [2, 1][usize::from(outer != 0)],
                                    2 => 1 + outer.min(2) as usize,
                                    3 => {
                                        if outer == 0 {
                                            4
                                        } else if inner == 0 {
                                            3
                                        } else {
                                            2
                                        }
                                    }
                                    4 | 5 => 1,
                                    6 => 0,
                                    _ => unreachable!(),
                                };
                                let undefined = poisoned == Some(0)
                                    || (shape == 3 && outer == 1 && poisoned == Some(1));
                                let mut state = ExecutionState::default();
                                let external = state.memory.alloc(8, 1);
                                state.memory.write(external, &[0xa5; 8]).unwrap();
                                let actual = interp::run_function_in_state(
                                    &c,
                                    f,
                                    arguments.clone(),
                                    &mut state,
                                    Some(512),
                                );
                                if undefined {
                                    assert!(
                                        matches!(actual, Err(interp::InterpError::Undefined(_))),
                                        "shape={shape} i{width} round={round} {arguments:?}: {actual:?}"
                                    );
                                } else {
                                    let actual = actual.unwrap();
                                    let expected = match (shape, outer, &arguments[selected]) {
                                        (6, _, _) => Value::Int(APInt::new(width, 1)),
                                        (4, 0, _) => Value::Int(APInt::new(width, 0)),
                                        (5, 1, Value::Int(value)) => Value::Int(APInt::new(
                                            width,
                                            value.to_u64().wrapping_mul(2),
                                        )),
                                        (_, _, value) => value.clone(),
                                    };
                                    match (&actual[..], &expected) {
                                        ([Value::Int(actual)], Value::Int(expected)) => {
                                            assert_eq!(
                                                actual.clone().with_signed(false),
                                                expected.clone().with_signed(false)
                                            )
                                        }
                                        ([Value::Poison], Value::Poison) => {}
                                        _ => panic!(
                                            "shape={shape} i{width} round={round} {arguments:?}: {actual:?}"
                                        ),
                                    }
                                }
                                assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
                                if let Some(index) = poisoned {
                                    arguments[index] = saved.unwrap();
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn gamma_private_cells_keep_unsupported_or_undemanded_memory() {
    let source = CELLS[0];
    for source in [
        source.replace("size = 4", "size = 1"),
        source.replace("state(%t)", "state(%t) {volatile = true}"),
        source.replace("state(%s2) :", "state(%s2) {unknown_contract = true} :"),
        source
            .replace("    %s1 = ptr.store %1, %3 state(%s0)\n", "")
            .replace("args(%s1)", "args(%s0)"),
        source.replace(
            "    %4, %s3",
            "    %escape = ptr.cmp %3, %3 {predicate = \"eq\"} : !i1\n    %4, %s3",
        ),
        source.replace(
            "size = 4, align = 4",
            "size = 4, align = 4, unknown_contract = true",
        ),
        source.replace(
            "module {",
            "module {layout.spec = #layout.spec<endianness = \"big\", !ptr.p = {size = 64, abi = 64}>} {",
        ),
        source.replace("-> %4, %s3", "-> %1, %s3"),
        source
            .replace(
                "    %4, %s3",
                "    %overwrite = ptr.store %1, %3 state(%s2)\n    %4, %s3",
            )
            .replace("ptr.load %3 state(%s2)", "ptr.load %3 state(%overwrite)"),
        CELLS[5].replace("-> %5, %s2", "-> %1, %s2"),
        CELLS[5].replace("state(%t) :", "state(%t) {volatile = true} :"),
        CELLS[5].replace("align = 4", "align = 4, unknown_contract = true"),
    ] {
        let (c, f, _, _) = fixture(&source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        let output = printed_memory(&c, f);
        assert!(output.contains("ptr.alloca"), "{source}\n{output}");
        if source.contains("state(%overwrite)") {
            assert!(
                output.contains("scf.switch") && output.contains("ptr.store"),
                "{output}"
            );
        } else {
            assert!(output.contains("ptr.load"), "{source}\n{output}");
        }
    }
    for result in 0..4 {
        let source = if result == 3 { CELLS[5] } else { source };
        let (c, f, _, _) = fixture(source);
        let ops = c.get_region(c.get_op(f).regions()[0]).op_ids();
        let operation = match result {
            0 => &tir::ptr::ALLOCA,
            1 | 2 => &tir::ptr::LOAD,
            _ => &tir::scf::SWITCH,
        };
        let op = ops
            .iter()
            .find(|&&id| c.get_op(id).is_operation(operation))
            .unwrap();
        let value = match result {
            0 => c.get_op(*op).results()[0],
            1 => c.get_op(*op).value_results()[0],
            _ => c.get_op(*op).state_results()[0],
        };
        let mut attrs = c.get_op(f).attributes();
        attrs.push(c.named_attribute(
            "keep",
            tir::attributes::AttributeValue::Array(
                vec![tir::attributes::AttributeValue::Value(value)].into(),
            ),
        ));
        c.set_op_attributes(f, attrs);
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        assert!(printed_memory(&c, f).contains("ptr.alloca"));
        assert!(printed_memory(&c, f).contains("ptr.load"));
        assert!(c.get_value(value).defining_op().is_some());
    }
}

#[test]
fn gamma_private_cells_preserve_faults_and_unused_controller_demand() {
    for (source, fault) in [
        (CELLS[0].replace("size = 4", "size = 1"), true),
        (CELLS[0].replace("-> %4, %s3", "-> %1, %s3"), false),
        (CELLS[5].replace("-> %5, %s2", "-> %1, %s2"), false),
        (
            CELLS[0]
                .replace(
                    "    %4, %s3",
                    "    %overwrite = ptr.store %1, %3 state(%s2)\n    %4, %s3",
                )
                .replace("ptr.load %3 state(%s2)", "ptr.load %3 state(%overwrite)"),
            false,
        ),
    ] {
        let (c, f, _, _) = fixture(&source);
        for round in 0..=1 {
            if round > 0 {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &AnalysisManager::new())
                    .unwrap();
            }
            for selector in [
                Value::Int(APInt::new(1, 0)),
                Value::Int(APInt::new(1, 1)),
                Value::Poison,
            ] {
                let poisoned = matches!(selector, Value::Poison);
                if fault && poisoned {
                    continue;
                }
                let result = interp::run_function(
                    &c,
                    f,
                    if source.contains("@forward(") {
                        vec![selector, Value::Int(APInt::new(32, 7))]
                    } else {
                        vec![
                            selector,
                            Value::Int(APInt::new(32, 7)),
                            Value::Int(APInt::new(32, 11)),
                        ]
                    },
                );
                if fault {
                    assert!(
                        matches!(result, Err(interp::InterpError::OutOfBounds { .. })),
                        "{result:?}"
                    );
                } else if poisoned {
                    assert!(
                        matches!(result, Err(interp::InterpError::Undefined(_))),
                        "{result:?}"
                    );
                } else {
                    assert!(
                        matches!(&result.unwrap()[..], [Value::Int(value)] if *value == APInt::new(32, 7))
                    );
                }
            }
        }
    }
}

#[test]
fn gamma_private_cells_preserve_discarded_value_faults() {
    let source = CELLS[0].replace(
        "    %s1 = ptr.store %1",
        "    %bad = divsi %1, %2 : !i32\n    %s1 = ptr.store %bad",
    );
    let (c, f, _, _) = fixture(&source);
    for round in 0..2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
        }
        let result = interp::run_function(
            &c,
            f,
            vec![
                Value::Int(APInt::new(1, 1)),
                Value::Int(APInt::new(32, 8)),
                Value::Int(APInt::new(32, 0)),
            ],
        );
        assert!(
            matches!(result, Err(interp::InterpError::Undefined(_))),
            "{result:?}"
        );
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
    }
}

#[test]
fn gamma_sibling_demand_preserves_partial_values_and_controller_faults() {
    for (source, promoted, selected_fault) in [
        (
            CELLS[5].replace(
                "%s1 = ptr.store %1,",
                "%zero = constant {value = 0} : !i32\n    %bad = divsi %1, %zero : !i32\n    %s1 = ptr.store %bad,",
            ),
            false,
            false,
        ),
        (
            CELLS[5].replace(
                "%4 = addi %3, %3 : !i32",
                "%zero = constant {value = 0} : !i32\n      %4 = divsi %3, %zero : !i32",
            ),
            true,
            true,
        ),
        (
            CELLS[5].replace(
                "%5, %s2 = scf.switch %0",
                "%zero = constant {value = 0} : !i32\n    %bad = divsi %1, %zero : !i32\n    %test = cmpi %bad, %1 {predicate = \"eq\"} : !i1\n    %5, %s2 = scf.switch %test",
            ),
            true,
            false,
        ),
    ] {
        let (c, f, _, _) = fixture(&source);
        for round in 0..=2 {
            if round > 0 {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &AnalysisManager::new())
                    .unwrap();
                assert_eq!(!printed_memory(&c, f).contains("ptr."), promoted);
            }
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            for selector in [Some(0), Some(1), None] {
                let mut state = ExecutionState::default();
                let external = state.memory.alloc(8, 1);
                state.memory.write(external, &[0xa5; 8]).unwrap();
                let result = interp::run_function_in_state(
                    &c,
                    f,
                    vec![
                        selector.map_or(Value::Poison, |value| Value::Int(APInt::new(1, value))),
                        Value::Int(APInt::new(32, 7)),
                    ],
                    &mut state,
                    Some(512),
                );
                if selected_fault && selector == Some(0) {
                    assert!(matches!(&result.unwrap()[..], [Value::Int(value)] if *value == APInt::new(32, 7)));
                } else {
                    assert!(matches!(result, Err(interp::InterpError::Undefined(_))), "{source}\n{result:?}");
                }
                assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
            }
        }
    }
}

#[test]
fn gamma_cell_pairs_require_closed_initialized_ordinary_cells() {
    let source = CELLS[6];
    for (source, expected) in [
        (
            source
                .replace("    %s1 = ptr.store %3, %1 state(%s0)\n", "")
                .replace("state(%s1)", "state(%s0)"),
            "poison",
        ),
        (source.replace("size = 4", "size = 1"), "bounds"),
        (
            source.replace("state(%e1)", "state(%e1) {volatile = true}"),
            "one",
        ),
        (
            source.replace("state(%s4) :", "state(%s4) {unknown_contract = true} :"),
            "one",
        ),
        (source.replace("-> %12, %s5", "-> %3, %s5"), "zero"),
        (
            source
                .replace("%s0 = state.entry_state", "%third = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>\n    %s0 = state.entry_state")
                .replace("%s3 = scf.switch %0 args(%s2)", "%s2x = ptr.store %3, %third state(%s2)\n    %s3 = scf.switch %0 args(%s2x)")
                .replace("-> %12, %s5", "%thirdv, %s6 = ptr.load %third state(%s5) : !i32\n    %sum3 = addi %12, %thirdv : !i32\n    -> %sum3, %s6"),
            "one",
        ),
        (
            source.replace(
                "%s0 = state.entry_state",
                "%foreign = ptr.alloca {size = 8, align = 8} : !ptr.p\n    %s0 = state.entry_state",
            ),
            "one",
        ),
        (
            source.replace(
                "%s0 = state.entry_state",
                "%escape = ptr.cmp %1, %1 {predicate = \"eq\"} : !i1\n    %s0 = state.entry_state",
            ),
            "one",
        ),
    ] {
        check_gamma_pair(&source, false, expected);
    }
}

#[test]
fn gamma_cell_pairs_preserve_poison_and_selected_partial_values() {
    let source = CELLS[6];
    for (source, promoted, expected) in [
        (source.replace("value = 0", "value = 2147483647").replace("addi %7, %8 :", "addi %7, %8 {nsw = true} :").replace("addi %4, %5 :", "addi %4, %5 {nsw = true} :"), true, "poison"),
        (source.replace("%9 = addi %7, %8 :", "%9 = divsi %7, %3 :"), true, "arm-fault"),
        (source.replace("%s1 = ptr.store %3,", "%bad = divsi %3, %3 : !i32\n    %s1 = ptr.store %bad,"), false, "fault"),
        (source.replace("%s3 = scf.switch %0", "%bad = divsi %3, %3 : !i32\n    %test = cmpi %bad, %3 {predicate = \"eq\"} : !i1\n    %s3 = scf.switch %test"), true, "fault"),
        (source.replace("%9 = addi %7, %8 :", "%9 = divsi %7, %3 :").replace("-> %e2", "%e3 = ptr.store %8, %2 state(%e2)\n      -> %e3"), false, "arm-fault"),
    ] {
        check_gamma_pair(&source, promoted, expected);
    }
}

fn check_gamma_pair(source: &str, promoted: bool, expected: &str) {
    let (c, f, _, _) = fixture(source);
    for round in 0..=2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
            let output = printed_memory(&c, f);
            assert_eq!(!output.contains("ptr."), promoted, "{source}\n{output}");
            if !promoted {
                assert_eq!(
                    output.matches("ptr.alloca").count(),
                    source.matches("ptr.alloca").count(),
                    "{output}"
                );
                assert!(output.contains("ptr.load"), "{output}");
            }
        }
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        for selector in [Some(0), Some(1), None] {
            let mut state = ExecutionState::default();
            let external = state.memory.alloc(8, 1);
            state.memory.write(external, &[0xa5; 8]).unwrap();
            let result = interp::run_function_in_state(
                &c,
                f,
                vec![selector.map_or(Value::Poison, |bits| Value::Int(APInt::new(1, bits)))],
                &mut state,
                Some(512),
            );
            match (expected, selector) {
                ("bounds", _) => assert!(
                    matches!(
                        result,
                        Err(interp::InterpError::OutOfBounds { size: 4, .. })
                    ),
                    "{result:?}"
                ),
                ("fault", _) | (_, None) | ("arm-fault", Some(0)) => assert!(
                    matches!(result, Err(interp::InterpError::Undefined(_))),
                    "{result:?}"
                ),
                ("poison", _) => assert!(matches!(&result.unwrap()[..], [Value::Poison])),
                ("zero", _) => assert!(
                    matches!(&result.unwrap()[..], [Value::Int(value)] if value.to_u64() == 0)
                ),
                _ => assert!(
                    matches!(&result.unwrap()[..], [Value::Int(value)] if value.to_u64() == 1)
                ),
            }
            assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
        }
    }
}
