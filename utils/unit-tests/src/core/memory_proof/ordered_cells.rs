use super::*;
use tir::{AnalysisManager, Pass};

const CELLS: [&str; 4] = [
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_for_iter_arg.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_invariant_slot.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_after_dead_store.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_pointer_iter_arg.tir"),
];

fn optimize(c: &Context, f: OpId) -> String {
    tir::passes::EqSatPass::new()
        .run(f, c, &AnalysisManager::new())
        .unwrap();
    tir::verify_op_tree(c, c.parent_op(f).unwrap()).unwrap();
    printed_memory(c, f)
}

fn arguments(
    shape: usize,
    width: u64,
    lower: i64,
    upper: i64,
    step: i64,
    initial: u64,
    accumulator: u64,
) -> Vec<Value> {
    let mut values = vec![
        Value::Int(APInt::new(64, lower as u64)),
        Value::Int(APInt::new(64, upper as u64)),
        Value::Int(APInt::new(64, step as u64)),
        Value::Int(APInt::new(width, initial)),
    ];
    if shape == 3 {
        values[3] = Value::Ptr(initial);
    }
    if shape == 1 || shape == 3 {
        values.push(Value::Int(APInt::new(width, accumulator)));
    }
    values
}

fn expected_ordered_value(
    shape: usize,
    width: u64,
    iterations: u64,
    initial: u64,
    accumulator: u64,
    poisoned: Option<usize>,
) -> Option<Value> {
    if poisoned.is_some_and(|slot| slot < 3) {
        return None;
    }
    let poison_is_demanded = match (shape, poisoned) {
        (0 | 3, Some(3)) | (1, Some(4)) => true,
        (1, Some(3)) | (3, Some(4)) => iterations > 0,
        (2, Some(3)) => iterations == 0,
        _ => false,
    };
    if poison_is_demanded {
        return Some(Value::Poison);
    }
    let bits = match shape {
        0 => initial.wrapping_mul(1 << iterations),
        1 => accumulator.wrapping_add(initial.wrapping_mul(iterations)),
        2 if iterations == 0 => initial.wrapping_add(7),
        2 => 43,
        3 => initial.wrapping_add(accumulator.wrapping_mul(iterations)),
        _ => unreachable!(),
    };
    Some(if shape == 3 {
        Value::Ptr(bits)
    } else {
        Value::Int(APInt::new(width, bits))
    })
}

#[test]
fn ordered_private_cells_preserve_finite_iterations_boundaries_and_poison() {
    for (shape, original) in CELLS.iter().enumerate() {
        for &width in if shape == 3 {
            &[64][..]
        } else {
            &[8, 16, 32, 64][..]
        } {
            let source = original.replace("!i32", &format!("!i{width}")).replace(
                "size = 4, align = 4",
                &format!("size = {}, align = {}", width / 8, width / 8),
            );
            let (c, f, _, _) = fixture(&source);
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            for round in 0..=2 {
                if round > 0 {
                    let output = optimize(&c, f);
                    assert!(
                        !output.contains(if shape == 3 { "ptr.alloca" } else { "ptr." }),
                        "shape={shape} i{width}\n{output}"
                    );
                    if width == 32 || shape == 3 {
                        filecheck::verify(
                            &filecheck::Source::new("ordered cell", *original),
                            &filecheck::Source::new("optimized", &output),
                            &filecheck::Config::default(),
                        )
                        .unwrap_or_else(|error| panic!("{error}\n{output}"));
                    }
                }
                for (lower, upper, step, iterations) in [
                    (0, 0, 1, 0),
                    (5, 2, 1, 0),
                    (-3, 2, 1, 5),
                    (0, 5, 2, 3),
                    (5, 7, 1, 2),
                ] {
                    for initial in [0, 1, 7, u64::MAX, 1 << (width - 1)] {
                        let accumulator = 13u64;
                        let args =
                            arguments(shape, width, lower, upper, step, initial, accumulator);
                        for poisoned in (0..args.len()).map(Some).chain([None]) {
                            let mut args = args.clone();
                            if let Some(slot) = poisoned {
                                args[slot] = Value::Poison;
                            }
                            let mut state = ExecutionState::default();
                            let external = state.memory.alloc(8, 1);
                            state.memory.write(external, &[0xa5; 8]).unwrap();
                            let actual = interp::run_function_in_state(
                                &c,
                                f,
                                args.clone(),
                                &mut state,
                                Some(512),
                            );
                            match (actual, expected_ordered_value(shape, width, iterations, initial, accumulator, poisoned)) {
                                (Err(interp::InterpError::Message(_)), None) => {},
                                (Ok(values), Some(expected)) => {
                                    let [value] = values.as_slice() else { panic!("expected one result: {values:?}") };
                                    match (value, expected) {
                                        (Value::Int(actual), Value::Int(expected)) => assert_eq!(actual.with_signed(false), expected),
                                        (Value::Ptr(actual), Value::Ptr(expected)) => assert_eq!(*actual, expected),
                                        (Value::Poison, Value::Poison) => {},
                                        (actual, expected) => panic!("shape={shape} i{width} round={round} {args:?}: {actual:?}, expected {expected:?}"),
                                    }
                                },
                                (actual, expected) => panic!("shape={shape} i{width} round={round} {args:?}: {actual:?}, expected {expected:?}"),
                            }
                            assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn ordered_private_cells_keep_unsupported_accesses_layouts_and_escapes() {
    let source = CELLS[0];
    let partial_load = source.replace(
        "%5 = ptr.load %4 : !i32",
        "%small = ptr.load %4 : !i16\n      %5 = extui %small : !i32",
    );
    let partial_store = source.replace(
        "ptr.store %6, %4",
        "%small = trunci %6 : !i16\n      ptr.store %small, %4",
    );
    let foreign = source
        .replace("%3: !i32) ->", "%3: !i32, %foreign: !ptr.p<!i32>) ->")
        .replace(
            "    func.return %7",
            "    %foreign_value = ptr.load %foreign : !i32\n    func.return %7",
        );
    for source in [
        source.replace("size = 4", "size = 1"),
        source.replace("ptr.store %6, %4", "ptr.store %6, %4 {volatile = true}"),
        source.replace("ptr.load %4 : !i32", "ptr.load %4 {unknown_contract = true} : !i32"),
        source.replace("size = 4, align = 4", "size = 4, align = 4, unknown_contract = true"),
        source.replace("    ptr.store %3, %4\n", ""),
        partial_load,
        partial_store,
        source.replace("      scf.yield", "      %extra = ptr.load %4 {volatile = true} : !i32\n      scf.yield"),
        source.replace(") -> !i32", ") -> !ptr.p<!i32>").replace("func.return %7", "func.return %4"),
        source.replace("module {", "module {layout.spec = #layout.spec<endianness = \"big\", !ptr.p = {size = 64, abi = 64}>} {"),
        source.replace("    func.return %7", "    %observe = ptr.load %4 {volatile = true} : !i32\n    func.return %7"),
        foreign,
    ] {
        let (c, f, _, _) = fixture(&source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        let output = optimize(&c, f);
        assert!(output.contains("ptr.alloca") && output.contains("ptr.load") && output.contains("scf.ordered_for"), "{source}\n{output}");
    }
    let interior = source.replace(
        "%4 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>",
        "%base = ptr.alloca {size = 8, align = 8} : !ptr.p<!i32>\n    %offset = constant {value = 4} : !i64\n    %4 = ptr.ptradd %base, %offset : !ptr.p<!i32>",
    );
    for (source, selected) in [(source, 0), (source, 1), (interior.as_str(), 0)] {
        let (c, f, _, _) = fixture(source);
        let ops = c
            .get_region(c.get_op(f).regions()[0])
            .iter(c.clone())
            .next()
            .unwrap()
            .op_ids();
        let op = ops
            .iter()
            .find(|&&id| {
                c.get_op(id).is_operation(if selected == 0 {
                    &tir::ptr::ALLOCA
                } else {
                    &tir::ptr::LOAD
                })
            })
            .unwrap();
        let value = c.get_op(*op).value_results()[0];
        let mut attrs = c.get_op(f).attributes();
        attrs.push(c.named_attribute(
            "keep",
            tir::attributes::AttributeValue::Array(
                vec![tir::attributes::AttributeValue::Value(value)].into(),
            ),
        ));
        c.set_op_attributes(f, attrs);
        let output = optimize(&c, f);
        assert!(
            output.contains("ptr.alloca") && output.contains("ptr.load"),
            "{output}"
        );
        assert!(c.get_value(value).defining_op().is_some());
        if source.contains("ptr.ptradd") {
            assert!(output.contains("ptr.ptradd"), "{output}");
        }
    }
}

#[test]
fn ordered_private_cells_reuse_only_unobserved_slots_and_preserve_faults() {
    let source = CELLS[2];
    let flagged_slot = source.replace("value = 3}", "value = 3, keep = true}");
    let used_slot = source.replace("constant {value = 36} : !i32", "addi %5, %5 : !i32");
    let flagged_write = source.replace("value = 36}", "value = 36, keep = true}");
    let eager = source
        .replace(
            "    %i1_end",
            "    %zero = constant {value = 0} : !i32\n    %i1_end",
        )
        .replace(
            "      %5 = constant",
            "      %fault = divsi %3, %zero : !i32\n      %5 = constant",
        );
    let standard = CELLS[0].replace(
        "      %5 = ptr.load",
        "      %spare = constant {value = 123} : !i32\n      %5 = ptr.load",
    );
    let ambiguous = source.replace(
        "      %5 = constant",
        "      %spare = constant {value = 123} : !i32\n      %5 = constant",
    );
    for (case, source) in [
        flagged_slot,
        used_slot,
        flagged_write,
        eager,
        standard,
        ambiguous,
    ]
    .iter()
    .enumerate()
    {
        let (c, f, _, _) = fixture(source);
        for round in 0..=1 {
            if round > 0 {
                let output = optimize(&c, f);
                assert_eq!(
                    output.contains("ptr."),
                    matches!(case, 0 | 1 | 2 | 5),
                    "{output}"
                );
                if case == 3 {
                    assert!(output.contains("divsi"), "{output}");
                }
            }
            for upper in [0, 3] {
                let mut state = ExecutionState::default();
                let external = state.memory.alloc(8, 1);
                state.memory.write(external, &[0xa5; 8]).unwrap();
                let actual = interp::run_function_in_state(
                    &c,
                    f,
                    arguments(2, 32, 0, upper, 1, 7, 0),
                    &mut state,
                    Some(512),
                );
                if case == 3 && upper > 0 {
                    assert!(
                        matches!(actual, Err(interp::InterpError::Undefined(_))),
                        "{actual:?}"
                    );
                } else {
                    let expected = if case == 4 {
                        if upper == 0 {
                            7
                        } else {
                            56
                        }
                    } else if upper == 0 {
                        14
                    } else if case == 1 {
                        13
                    } else {
                        43
                    };
                    assert!(
                        matches!(&actual.unwrap()[..], [Value::Int(value)] if value.to_u64() == expected)
                    );
                }
                assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
            }
        }
    }
    let (c, f, _, _) = fixture(source);
    let owner = c
        .get_region(c.get_op(f).regions()[0])
        .iter(c.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .find(|id| c.get_op(*id).name().as_str() == "ordered_for")
        .unwrap();
    let body = c.get_region(c.get_op(owner).regions()[0]);
    let slot = body.iter(c.clone()).next().unwrap().op_ids()[0];
    let value = c.get_op(slot).value_results()[0];
    let mut attrs = c.get_op(f).attributes();
    attrs.push(c.named_attribute("keep", tir::attributes::AttributeValue::Value(value)));
    c.set_op_attributes(f, attrs);
    let output = optimize(&c, f);
    assert!(
        output.contains("ptr.alloca") && output.contains("ptr.load"),
        "{output}"
    );
    assert_eq!(c.get_value(value).defining_op(), Some(slot));
}

#[test]
fn ordered_private_cells_retain_partial_producers_and_eager_foreign_faults() {
    let source = CELLS[0];
    let partial = source
        .replace(
            "    %i1_end",
            "    %zero = constant {value = 0} : !i32\n    %i1_end",
        )
        .replace("addi %5, %5", "divsi %5, %zero")
        .replace("func.return %7", "func.return %3");
    let foreign = source
        .replace("%3: !i32) ->", "%3: !i32, %foreign: !ptr.p<!i32>) ->")
        .replace(
            "    func.return %7",
            "    %foreign_value = ptr.load %foreign : !i32\n    func.return %7",
        );
    for (case, source) in [partial, foreign].iter().enumerate() {
        let (c, f, _, _) = fixture(source);
        for round in 0..=1 {
            if round > 0 {
                let output = optimize(&c, f);
                if case == 0 {
                    assert!(
                        output.contains("divsi") && !output.contains("ptr."),
                        "{output}"
                    );
                } else {
                    assert!(
                        output.contains("ptr.load") && output.contains("ptr.alloca"),
                        "{output}"
                    );
                }
            }
            for upper in [0, 3] {
                for &alias in &[false, true][..if case == 0 { 1 } else { 2 }] {
                    let mut args = arguments(0, 32, 0, upper, 1, 7, 0);
                    if case == 1 {
                        let address = ExecutionState::default().memory.alloc(4, 4);
                        args.push(Value::Ptr(if alias { address } else { 0 }));
                    }
                    let actual = interp::run_function_within(&c, f, args, Some(512));
                    if case == 0 && upper > 0 {
                        assert!(
                            matches!(actual, Err(interp::InterpError::Undefined(_))),
                            "case={case} round={round} upper={upper} alias={alias}: {actual:?}"
                        );
                    } else if case == 1 && !alias {
                        assert!(
                            matches!(actual, Err(interp::InterpError::OutOfBounds { .. })),
                            "{actual:?}"
                        );
                    } else {
                        let expected = if upper == 0 || case == 0 { 7 } else { 56 };
                        assert!(
                            matches!(&actual.unwrap()[..], [Value::Int(value)] if value.to_u64() == expected)
                        );
                    }
                }
            }
        }
    }
}
