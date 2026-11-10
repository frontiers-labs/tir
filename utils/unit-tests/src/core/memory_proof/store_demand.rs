use super::*;
use tir::{AnalysisManager, Pass};

#[test]
fn graph_overwritten_store_preserves_demanded_fault_and_memory() {
    let source = include_str!("../../../../../core/checks/Inputs/eqsat/dead_store.tir")
        .replace(
            "    %4 = ptr.store %1,",
            "    %fault = divsi %1, %2 : !i32\n    %4 = ptr.store %fault,",
        )
        .replace("func.return state(%5)", "-> %5");
    let (c, f, _, _) = fixture(&source);
    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
    for round in 0..=2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        }
        let mut state = ExecutionState::default();
        let external = state.memory.alloc(4, 1);
        state.memory.write(external, &[0xa5; 4]).unwrap();
        let result = interp::run_function_in_state(
            &c,
            f,
            vec![
                Value::Ptr(external),
                Value::Int(APInt::new(32, 7)),
                Value::Int(APInt::new(32, 0)),
            ],
            &mut state,
            None,
        );
        assert!(
            matches!(result, Err(interp::InterpError::Undefined(_))),
            "round={round}: {result:?}\n{}",
            printed_memory(&c, f)
        );
        assert_eq!(state.memory.read(external, 4).unwrap(), [0xa5; 4]);
    }
}

#[test]
fn graph_overwrites_keep_the_store_selected_by_each_arm() {
    let source =
        include_str!("../../../../../core/checks/Inputs/eqsat/dead_store_under_a_gate.tir");
    let c = Context::with_default_dialects();
    let module = tir::parse::ir::parse_operation(&c, source, &MODULE).unwrap();
    let f = c.get_region(module.regions()[0]).op_ids()[0];
    tir::verify_op_tree(&c, module.id()).unwrap();
    for round in 0..=2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            let output = printed_memory(&c, f);
            filecheck::verify(
                &filecheck::Source::new("arm overwrites", source),
                &filecheck::Source::new("optimized", &output),
                &filecheck::Config::default(),
            )
            .unwrap();
            assert_eq!(output.matches("ptr.store").count(), 2, "{output}");
        }
        for condition in [Some(0), Some(1), None] {
            for first in [Some(17), None] {
                for last in [Some(29), None] {
                    let mut state = ExecutionState::default();
                    let pointer = state.memory.alloc(4, 4);
                    state.memory.write(pointer, &[0xa5; 4]).unwrap();
                    let value = |bits: Option<u64>| {
                        bits.map_or(Value::Poison, |n| Value::Int(APInt::new(32, n)))
                    };
                    let result = interp::run_function_in_state(
                        &c,
                        f,
                        vec![
                            condition.map_or(Value::Poison, |n| Value::Int(APInt::new(1, n))),
                            Value::Ptr(pointer),
                            value(first),
                            value(last),
                        ],
                        &mut state,
                        Some(512),
                    );
                    if condition.is_none() {
                        assert!(
                            matches!(result, Err(interp::InterpError::Undefined(_))),
                            "{result:?}"
                        );
                        assert_eq!(state.memory.read(pointer, 4).unwrap(), [0xa5; 4]);
                    } else {
                        result.unwrap();
                        let selected = if condition == Some(0) { first } else { last };
                        if let Some(bits) = selected {
                            assert_eq!(
                                state.memory.read(pointer, 4).unwrap(),
                                (bits as u32).to_le_bytes()
                            );
                        } else {
                            assert!(state.memory.read(pointer, 4).is_err());
                        }
                    }
                }
            }
        }
    }
}
