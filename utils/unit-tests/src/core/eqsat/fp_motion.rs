use super::*;
use tir::utils::APFloat;

#[test]
fn fp_motion_preserves_path_results_poison_control_and_ignored_flags() {
    enum Expected {
        Float(u128),
        Integer(u64),
        QuietNaN(u32),
        IndeterminateInteger,
    }
    use Expected::{Float, IndeterminateInteger, Integer, QuietNaN};
    let fixtures = [
        (
            include_str!("../../../../../core/checks/Inputs/eqsat/fp-ignore-hoist.tir"),
            "add",
            vec![
                (
                    vec![0x3ff0_0000_0000_0000, 0x4000_0000_0000_0000],
                    Float(0x4008_0000_0000_0000),
                ),
                (vec![0x8000_0000_0000_0000; 2], Float(0x8000_0000_0000_0000)),
                (vec![1, 1], Float(2)),
                (
                    vec![0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000],
                    QuietNaN(64),
                ),
                (vec![0x7ff8_0000_0000_1234, 0], QuietNaN(64)),
                (vec![0x7ff0_0000_0000_0001, 0], QuietNaN(64)),
            ],
        ),
        (
            include_str!("../../../../../core/checks/Inputs/eqsat/fp-ignore-compare-hoist.tir"),
            "cmp",
            vec![
                (
                    vec![0x3ff0_0000_0000_0000, 0x4000_0000_0000_0000],
                    Integer(1),
                ),
                (
                    vec![0x4000_0000_0000_0000, 0x3ff0_0000_0000_0000],
                    Integer(0),
                ),
                (vec![0x8000_0000_0000_0000, 0], Integer(0)),
                (vec![0x7ff8_0000_0000_1234, 0], Integer(0)),
                (vec![0x7ff0_0000_0000_0001, 0], Integer(0)),
            ],
        ),
        (
            include_str!("../../../../../core/checks/Inputs/eqsat/fp-ignore-convert-hoist.tir"),
            "convert",
            vec![
                (vec![0x3ff8_0000_0000_0000], Float(0x3fc0_0000)),
                (vec![0x8000_0000_0000_0000], Float(0x8000_0000)),
                (vec![0x7fef_ffff_ffff_ffff], Float(0x7f80_0000)),
                (vec![1], Float(0)),
                (vec![0x7ff8_0000_0000_1234], QuietNaN(32)),
                (vec![0x7ff0_0000_0000_0001], QuietNaN(32)),
            ],
        ),
        (
            include_str!("../../../../../core/checks/Inputs/eqsat/fp-ignore-integer-hoist.tir"),
            "to_si",
            vec![
                (vec![0x3ffc_0000_0000_0000], Integer(1)),
                (vec![0xbffc_0000_0000_0000], Integer(0xffff_ffff)),
                (vec![0x41df_ffff_ffc0_0000], Integer(0x7fff_ffff)),
                (vec![0xc1e0_0000_0000_0000], Integer(0x8000_0000)),
                (vec![0x7ff0_0000_0000_0000], IndeterminateInteger),
                (vec![0x7ff8_0000_0000_1234], IndeterminateInteger),
            ],
        ),
    ];
    for (source, name, cases) in fixtures {
        let (context, module, function, _, _) = parse(source);
        let body = context.get_region(context.get_op(function).regions()[0]);
        let float = |bits| Value::Float(APFloat::from_bits(11, 52, false, bits));
        for optimized in [false, true] {
            if optimized {
                EqSatPass::new()
                    .run(function, &context, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&context, module.id()).unwrap();
                let entry = body.iter(context.clone()).next().unwrap();
                let ops = entry.op_ids();
                assert_eq!(context.get_op(ops[0]).name().as_str(), name);
                assert_eq!(context.get_op(ops[1]).name().as_str(), "cond_br");
                assert_eq!(
                    printed_ir(&context, &module)
                        .matches(&format!("= fp.{name} "))
                        .count(),
                    1
                );
            }
            for condition in [0, 1] {
                for (bits, expected) in &cases {
                    let mut args = vec![Value::Int(APInt::new(1, condition))];
                    args.extend(bits.iter().copied().map(float));
                    for initial in [0, 31] {
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
                        match (actual.as_slice(), expected) {
                            ([Value::Float(actual)], Float(expected)) => {
                                assert_eq!(actual.to_bits(), *expected)
                            }
                            ([Value::Int(actual)], Integer(expected)) => {
                                assert_eq!(actual.to_u64(), *expected)
                            }
                            ([Value::Float(actual)], QuietNaN(width)) => {
                                let (exponent, quiet) = if *width == 64 {
                                    (0x7ff0_0000_0000_0000, 0x0008_0000_0000_0000)
                                } else {
                                    (0x7f80_0000, 0x0040_0000)
                                };
                                assert_eq!(actual.to_bits() & (exponent | quiet), exponent | quiet);
                            }
                            ([Value::Int(actual)], IndeterminateInteger) => {
                                assert_eq!(actual.width(), 32)
                            }
                            _ => panic!("unexpected {name} result {actual:?}"),
                        }
                        assert_eq!(state.fp_environment.flags, initial);
                    }
                }
                for poisoned in 0..cases[0].0.len() {
                    let mut args = vec![Value::Int(APInt::new(1, condition))];
                    args.extend(cases[0].0.iter().copied().map(float));
                    args[poisoned + 1] = Value::Poison;
                    assert!(matches!(
                        interp::run_function(&context, function, args)
                            .unwrap()
                            .as_slice(),
                        [Value::Poison]
                    ));
                }
            }
            let mut args = vec![Value::Poison];
            args.extend(cases[0].0.iter().copied().map(float));
            assert!(matches!(
                interp::run_function(&context, function, args),
                Err(interp::InterpError::Undefined(_))
            ));
        }
    }
}

#[test]
fn fp_motion_requires_actual_speculation_and_matching_unobserved_evaluations() {
    struct NoSpeculation;
    impl tir::Speculatable for NoSpeculation {
        fn is_speculatable(&self) -> bool {
            false
        }
    }
    let addition = include_str!("../../../../../core/checks/Inputs/eqsat/fp-ignore-hoist.tir");
    let comparison =
        include_str!("../../../../../core/checks/Inputs/eqsat/fp-ignore-compare-hoist.tir");
    let integer = include_str!("../../../../../core/checks/Inputs/eqsat/nodes-trap-in-arm.tir");
    let sources = [
        (integer.to_owned(), Some("builtin.addi"), false),
        (
            integer.replace("addi %x, %x : !i32", "addi %x, %x {retained = true} : !i32"),
            None,
            false,
        ),
        (
            integer.replace("%b = addi %x, %x", "%b = addi %d, %d"),
            None,
            false,
        ),
        (
            integer.replace("%q = divsi %x, %d", "%q = divsi %a, %d"),
            None,
            false,
        ),
        (integer.to_owned(), None, true),
        (integer.replace("addi %x, %x", "divsi %x, %x"), None, false),
        (addition.to_owned(), Some("fp.add"), false),
        (comparison.to_owned(), Some("fp.cmp"), false),
        (
            include_str!("../../../../../core/checks/Inputs/eqsat/fp-ignore-convert-hoist.tir")
                .to_owned(),
            Some("fp.convert"),
            false,
        ),
        (
            include_str!("../../../../../core/checks/Inputs/eqsat/fp-ignore-integer-hoist.tir")
                .to_owned(),
            Some("fp.to_si"),
            false,
        ),
        (
            addition.replace("%a, %b : !f64", "%a, %b {retained = true} : !f64"),
            None,
            false,
        ),
        (
            addition.replace("%y = fp.add %a, %b", "%y = fp.add %b, %b"),
            None,
            false,
        ),
        (
            comparison.replace(
                "%y = fp.cmp %a, %b {predicate = \"olt\"}",
                "%y = fp.cmp %a, %b {predicate = \"ogt\"}",
            ),
            None,
            false,
        ),
        (addition.to_owned(), None, true),
    ];
    for (source, no_speculation, metadata) in sources {
        let context = Context::with_default_dialects();
        if let Some(name) = no_speculation {
            let (dialect, name) = name.split_once('.').unwrap();
            context.register_op_interface::<dyn tir::Speculatable>(dialect, name, |_| {
                tir::erase_op_interface::<dyn tir::Speculatable>(Box::new(NoSpeculation))
            });
        }
        let module = parse_operation(&context, &source, &MODULE).unwrap();
        let function = context.get_region(module.regions()[0]).op_ids()[0];
        let body = context.get_region(context.get_op(function).regions()[0]);
        let blocks = body.iter(context.clone()).collect::<Vec<_>>();
        let first = blocks[1].op_ids()[0];
        let second = blocks[2].op_ids()[0];
        if metadata {
            let mut attrs = context.get_op(function).attributes();
            attrs.push(context.named_attribute(
                "observed",
                tir::attributes::AttributeValue::Value(context.get_op(second).results()[0]),
            ));
            context.set_op_attributes(function, attrs);
        }
        tir::verify_op_tree(&context, module.id()).unwrap();
        let before = printed_ir(&context, &module);
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&context, module.id()).unwrap();
        assert_eq!(printed_ir(&context, &module), before);
        assert!(blocks[1].op_ids().contains(&first));
        assert!(blocks[2].op_ids().contains(&second));
    }
}

#[test]
fn fp_motion_keeps_both_sequential_flagged_divisions() {
    let source = include_str!("../../../../../core/checks/Inputs/eqsat/fp-flags-cse.tir");
    let (context, module, function, _, _) = parse(source);
    EqSatPass::new()
        .run(function, &context, &AnalysisManager::new())
        .unwrap();
    tir::verify_op_tree(&context, module.id()).unwrap();
    assert_eq!(
        printed_ir(&context, &module).matches("= fp.div ").count(),
        2
    );
    for (a, b, expected) in [(1.0f64, 3.0f64, 1), (1.0, 0.0, 8)] {
        let args = [a, b]
            .map(|value| Value::Float(APFloat::from_bits(11, 52, false, value.to_bits().into())))
            .to_vec();
        let actual = interp::run_function(&context, function, args).unwrap();
        let [Value::Int(actual)] = actual.as_slice() else {
            panic!("expected flags")
        };
        assert_eq!(actual.to_u64(), expected);
    }
}
