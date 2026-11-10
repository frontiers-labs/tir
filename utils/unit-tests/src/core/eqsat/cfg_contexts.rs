use super::*;

#[test]
fn cfg_contexts_fold_dominated_comparisons_and_correlated_edges() {
    let fixtures = [
        (
            "branch_dominated",
            include_str!("../../../../../core/checks/Inputs/eqsat/branch_dominated.tir"),
            false,
        ),
        (
            "branch_dominated_nested",
            include_str!("../../../../../core/checks/Inputs/eqsat/branch_dominated_nested.tir"),
            false,
        ),
        (
            "correlated_branch",
            include_str!("../../../../../core/checks/Inputs/eqsat/correlated_branch.tir"),
            true,
        ),
    ];
    let mut failures = Vec::new();
    for (name, original, correlated) in fixtures {
        for false_edge in [false, true] {
            let source = if false_edge {
                original.replace("cfg.cond_br %0, ^bb1, ^bb2", "cfg.cond_br %0, ^bb2, ^bb1")
            } else {
                original.to_owned()
            };
            if false_edge && !correlated {
                continue;
            }
            let (context, module, function, _, _) = parse(&source);
            let body = context.get_region(context.get_op(function).regions()[0]);
            let entry = body.iter(context.clone()).next().unwrap();
            let comparison = entry.op_ids()[0];
            let controller = *entry.op_ids().last().unwrap();
            let comparison_attrs = context.get_op(comparison).attributes();
            let comparison_operands = context.get_op(comparison).operands();
            let mut pass = EqSatPass::new();
            for round in 0..=2 {
                if round > 0 {
                    pass.run(function, &context, &AnalysisManager::new())
                        .unwrap();
                    tir::verify_op_tree(&context, module.id()).unwrap();
                    let output = printed_ir(&context, &module);
                    let checks = if false_edge {
                        source
                            .replace(
                                "// CHECK: cfg.cond_br %[[C]], ^bb1, ^bb2",
                                "// CHECK: cfg.cond_br %[[C]], ^bb2, ^bb1",
                            )
                            .replace(
                                "// CHECK-NEXT: %[[ONE:[0-9]+]] = constant {value = 1}",
                                "// CHECK: %[[ONE:[0-9]+]] = constant {value = 2}",
                            )
                    } else {
                        source.clone()
                    };
                    if let Err(error) = filecheck::verify(
                        &filecheck::Source::new(name, &checks),
                        &filecheck::Source::new("optimized", &output),
                        &filecheck::Config::default(),
                    ) {
                        failures.push(format!(
                            "{name}, false edge {false_edge}: {error}\n{output}"
                        ));
                    }
                    assert!(context.get_op(comparison).is_operation(&tir::builtin::CMPI));
                    assert_eq!(context.get_op(comparison).attributes(), comparison_attrs);
                    assert_eq!(context.get_op(comparison).operands(), comparison_operands);
                    assert!(context.get_op(controller).is_operation(&tir::cfg::COND_BR));
                    assert_eq!(
                        context.get_op(controller).value_operands()[0],
                        context.get_op(comparison).results()[0]
                    );
                }
                for (a, b) in [(0, 0), (-1, -1), (-1, 0), (9, 17)] {
                    let result = interp::run_function(
                        &context,
                        function,
                        vec![
                            Value::Int(APInt::new_signed(32, a)),
                            Value::Int(APInt::new_signed(32, b)),
                        ],
                    )
                    .unwrap();
                    let [Value::Int(actual)] = result.as_slice() else {
                        panic!("expected integer result")
                    };
                    let expected = if correlated {
                        if (a == b) != false_edge {
                            if false_edge {
                                2
                            } else {
                                1
                            }
                        } else {
                            a
                        }
                    } else {
                        i64::from(a == b)
                    };
                    assert_eq!(
                        actual.clone().with_signed(false),
                        APInt::new_signed(if correlated { 32 } else { 1 }, expected)
                            .with_signed(false)
                    );
                }
                for poisoned in [0, 1, 2] {
                    let mut arguments = vec![Value::Int(APInt::new(32, 0)); 2];
                    if poisoned == 2 {
                        arguments.fill(Value::Poison);
                    } else {
                        arguments[poisoned] = Value::Poison;
                    }
                    assert!(matches!(
                        interp::run_function(&context, function, arguments),
                        Err(interp::InterpError::Undefined(_))
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn cfg_contexts_keep_join_cycle_and_metadata_comparisons() {
    let original = include_str!("../../../../../core/checks/Inputs/eqsat/branch_dominated.tir");
    for case in ["join", "cycle", "metadata"] {
        let source = match case {
            "join" => original.replace("  ^bb2:\n    func.return %c", "  ^bb2:\n    cfg.br ^bb1"),
            "cycle" => original
                .replace(
                    "    cfg.cond_br %c, ^bb1, ^bb2",
                    "    func.return %c\n  ^cycle:\n    cfg.br ^bb1",
                )
                .replace("    func.return %d", "    cfg.br ^cycle"),
            _ => original.replace(
                "%d = cmpi %a, %b {predicate",
                "%d = cmpi %a, %b {retained = \"comparison\", predicate",
            ),
        };
        let (context, module, function, _, _) = parse(&source);
        let body = context.get_region(context.get_op(function).regions()[0]);
        let comparisons = body
            .op_ids()
            .into_iter()
            .filter(|&op| context.get_op(op).is_operation(&tir::builtin::CMPI))
            .collect::<Vec<_>>();
        assert_eq!(comparisons.len(), 2);
        let attrs = comparisons
            .iter()
            .map(|&op| context.get_op(op).attributes())
            .collect::<Vec<_>>();
        let mut pass = EqSatPass::new();
        for _ in 0..2 {
            pass.run(function, &context, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&context, module.id()).unwrap();
            for (&comparison, attributes) in comparisons.iter().zip(&attrs) {
                assert!(
                    context.get_op(comparison).is_operation(&tir::builtin::CMPI),
                    "{case}"
                );
                assert_eq!(
                    &context.get_op(comparison).attributes(),
                    attributes,
                    "{case}"
                );
            }
        }
    }
}
