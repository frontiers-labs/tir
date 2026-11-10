use super::*;
use tir::{AnalysisManager, Pass};

const COHORT: [&str; 5] = [
    include_str!(
        "../../../../../core/checks/Inputs/eqsat/theta_load_condition_latched_do_while.tir"
    ),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_early_exit.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_break_under_gate.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_break_and_continue.tir"),
    include_str!("../../../../../core/checks/Inputs/eqsat/theta_load_exits_disagree.tir"),
];

fn bit(value: Option<u64>) -> Value {
    value.map_or(Value::Poison, |value| Value::Int(APInt::new(1, value)))
}

fn returned_bits(c: &Context, f: OpId, args: Vec<Value>, expected: Option<u64>) {
    let actual = interp::run_function_within(c, f, args, Some(1024)).unwrap();
    match (&actual[..], expected) {
        ([Value::Int(value)], Some(expected)) => assert_eq!(value.to_u64(), expected),
        ([Value::Poison], None) => {}
        _ => panic!("expected={expected:?}: {actual:?}"),
    }
}

fn optimize(c: &Context, f: OpId) -> String {
    tir::passes::EqSatPass::new()
        .run(f, c, &AnalysisManager::new())
        .unwrap();
    tir::verify_op_tree(c, c.parent_op(f).unwrap()).unwrap();
    let output = printed_memory(c, f);
    assert!(!output.contains("ptr."), "{output}");
    output
}

#[test]
fn theta_tail_and_local_switch_cells_preserve_finite_results_and_poison_demand() {
    for (shape, original) in COHORT[..4].iter().enumerate() {
        for width in [8, 16, 32, 64] {
            let source = original.replace("!i32", &format!("!i{width}")).replace(
                "size = 4, align = 4",
                &format!("size = {}, align = {}", width / 8, width / 8),
            );
            let (c, f, _, _) = fixture(&source);
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            for round in 0..=2 {
                if round > 0 {
                    let output = optimize(&c, f);
                    if width == 32 {
                        filecheck::verify(
                            &filecheck::Source::new("theta cohort", *original),
                            &filecheck::Source::new("optimized", &output),
                            &filecheck::Config::default(),
                        )
                        .unwrap_or_else(|error| panic!("{error}\n{output}"));
                    }
                }
                for selector in 0..=u64::from(shape == 1) {
                    for seed in [Some(0), Some(1), Some(7), Some(u64::MAX), None] {
                        let value =
                            seed.map_or(Value::Poison, |bits| Value::Int(APInt::new(width, bits)));
                        let expected = seed.map(|bits| {
                            APInt::new(
                                width,
                                if shape == 0 {
                                    bits.wrapping_mul(2)
                                } else {
                                    bits
                                },
                            )
                            .to_u64()
                        });
                        returned_bits(&c, f, vec![bit(Some(selector)), value], expected);
                    }
                }
                let result = interp::run_function_within(
                    &c,
                    f,
                    vec![Value::Poison, Value::Int(APInt::new(width, 7))],
                    Some(1024),
                );
                assert!(
                    matches!(result, Err(interp::InterpError::Undefined(_))),
                    "shape={shape} round={round}: {result:?}"
                );
            }
        }
    }
}

#[test]
fn theta_local_switch_exit_values_preserve_selector_demand() {
    let (c, f, _, _) = fixture(COHORT[4]);
    for round in 0..=2 {
        if round > 0 {
            let output = optimize(&c, f);
            filecheck::verify(
                &filecheck::Source::new("theta exits", COHORT[4]),
                &filecheck::Source::new("optimized", &output),
                &filecheck::Config::default(),
            )
            .unwrap_or_else(|error| panic!("{error}\n{output}"));
        }
        for outer in [Some(0), Some(1), None] {
            for inner in [Some(0), Some(1), None] {
                // The original has no finite return for outer=1, inner=0.
                if (outer, inner) == (Some(1), Some(0)) {
                    continue;
                }
                let args = vec![bit(outer), bit(inner)];
                if outer.is_none() || (outer == Some(1) && inner.is_none()) {
                    let result = interp::run_function_within(&c, f, args, Some(1024));
                    assert!(
                        matches!(result, Err(interp::InterpError::Undefined(_))),
                        "{result:?}"
                    );
                } else {
                    returned_bits(&c, f, args, Some(u64::from(outer == Some(1))));
                }
            }
        }
    }
}

#[test]
fn theta_tail_cell_carries_each_finite_iteration() {
    let source = COHORT[0].replace(
        "      -> %0, %10, %10",
        "      %limit = constant {value = 8} : !i32\n      %more = cmpi %9, %limit {predicate = \"ult\"} : !i1\n      -> %more, %10, %10",
    );
    let (c, f, _, _) = fixture(&source);
    for round in 0..=2 {
        if round > 0 {
            optimize(&c, f);
        }
        for (seed, expected) in [
            (1, 8),
            (2, 8),
            (4, 8),
            (8, 16),
            (16, 32),
            (u32::MAX as u64, u32::MAX as u64 - 1),
        ] {
            returned_bits(
                &c,
                f,
                vec![Value::Poison, Value::Int(APInt::new(32, seed))],
                Some(expected),
            );
        }
    }
}

#[test]
fn theta_local_switch_cells_carry_each_selected_store_to_a_finite_exit() {
    let gate = COHORT[2]
        .replace("%1: !i32)", "%1: !i32, %inner: !i1)")
        .replace("%16 = scf.switch %0", "%16 = scf.switch %inner")
        .replace("%18 = constant {value = 1}", "%18 = constant {value = 0}");
    let rollback = COHORT[3]
        .replace("%1: !i32)", "%1: !i32, %inner: !i1, %rollback: !i1)")
        .replace("%16, %17 = scf.switch %0", "%16, %17 = scf.switch %inner")
        .replace(
            "%20, %21 = scf.switch %0",
            "%20, %21 = scf.switch %rollback",
        )
        .replace("%23 = constant {value = 1}", "%23 = constant {value = 0}")
        .replace("%27 = constant {value = 1}", "%27 = constant {value = 0}");
    let exits = COHORT[4].replace("%19 = constant {value = 1}", "%19 = constant {value = 0}");
    for (shape, source) in [gate, rollback, exits].iter().enumerate() {
        let (c, f, _, _) = fixture(source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        for round in 0..=2 {
            if round > 0 {
                optimize(&c, f);
            }
            for outer in [Some(0), Some(1), None] {
                for inner in [Some(0), Some(1), None] {
                    for rollback in
                        [Some(0), Some(1), None]
                            .into_iter()
                            .take(if shape == 1 { 3 } else { 1 })
                    {
                        let mut args = vec![bit(outer)];
                        if shape < 2 {
                            args.push(Value::Int(APInt::new(32, 7)));
                        }
                        args.push(bit(inner));
                        if shape == 1 {
                            args.push(bit(rollback));
                        }
                        let undefined = outer.is_none()
                            || (outer == Some(1) && inner.is_none())
                            || (shape == 1
                                && outer == Some(1)
                                && inner == Some(0)
                                && rollback.is_none());
                        if undefined {
                            let result = interp::run_function_within(&c, f, args, Some(1024));
                            assert!(
                                matches!(result, Err(interp::InterpError::Undefined(_))),
                                "shape={shape} round={round}: {result:?}"
                            );
                        } else {
                            let expected = if shape == 2 {
                                u64::from(outer == Some(1) && inner == Some(1))
                            } else if outer == Some(1) && (shape == 0 || inner == Some(1)) {
                                14
                            } else {
                                7
                            };
                            returned_bits(&c, f, args, Some(expected));
                        }
                    }
                }
            }
        }
    }
}
