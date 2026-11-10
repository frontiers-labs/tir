use super::*;
use tir::{AnalysisManager, Pass};

const TWICE: &str =
    include_str!("../../../../../core/checks/Inputs/eqsat/identical_stores_are_one.tir");
const THRICE: &str =
    include_str!("../../../../../core/checks/Inputs/eqsat/three_stores_keep_the_last.tir");
const STORES: &str = r#"module {
  %f = func.func @f(%p: !ptr.p, %x: !i32, %y: !i32) -> !i32 {
    ptr.store %x, %p
    ptr.store %y, %p
    %v = ptr.load %p : !i32
    func.return %v
  }
  module_end
}"#;

fn implicit_fixture(source: &str) -> (Context, OpId) {
    let c = Context::with_default_dialects();
    let module = tir::parse::ir::parse_operation(&c, source, &tir::builtin::MODULE).unwrap();
    let function = c.get_region(module.regions()[0]).op_ids()[0];
    (c, function)
}

fn optimize(c: &Context, f: OpId, stores: usize) -> String {
    tir::passes::EqSatPass::new()
        .run(f, c, &AnalysisManager::new())
        .unwrap();
    tir::verify_op_tree(c, c.parent_op(f).unwrap()).unwrap();
    let output = printed_memory(c, f);
    assert_eq!(output.matches("ptr.store").count(), stores, "{output}");
    output
}

#[test]
fn identical_implicit_stores_are_one() {
    original_overwrite(TWICE, false);
}

#[test]
fn three_implicit_stores_keep_the_last() {
    original_overwrite(THRICE, true);
}

fn original_overwrite(source: &str, thrice: bool) {
    let (c, f) = implicit_fixture(source);
    for round in 0..=1 {
        if round > 0 {
            let output = optimize(&c, f, 1);
            assert!(!output.contains("state.entry_state"), "{output}");
            filecheck::verify(
                &filecheck::Source::new("implicit overwrite", source),
                &filecheck::Source::new("optimized", &output),
                &filecheck::Config::default(),
            )
            .unwrap();
        }
        for poison in [false, true] {
            let mut state = ExecutionState::default();
            let p = state.memory.alloc(8, 1);
            state.memory.write(p, &[0xa5; 8]).unwrap();
            let mut args = vec![Value::Ptr(p)];
            if !thrice {
                args.push(if poison {
                    Value::Poison
                } else {
                    Value::Int(APInt::new(32, 0x12345678))
                });
            }
            assert!(
                interp::run_function_in_state(&c, f, args, &mut state, Some(100))
                    .unwrap()
                    .is_empty()
            );
            if poison && !thrice {
                assert!(matches!(
                    state.memory.read(p, 4),
                    Err(interp::InterpError::Uninitialized { .. })
                ));
            } else {
                assert_eq!(
                    state.memory.read(p, 4).unwrap(),
                    if thrice {
                        [0; 4]
                    } else {
                        [0x78, 0x56, 0x34, 0x12]
                    }
                );
            }
            assert_eq!(state.memory.read(p + 4, 4).unwrap(), [0xa5; 4]);
        }
    }
}

#[test]
fn implicit_integer_overwrites_preserve_last_bits_poison_and_address_faults() {
    for width in [8, 16, 32, 64] {
        let source = STORES
            .replace(
                "ptr.store %x, %p",
                "ptr.store %x, %p\n    %pure = constant {value = 123} : !i32",
            )
            .replace("!i32", &format!("!i{width}"));
        let (c, f) = implicit_fixture(&source);
        for round in 0..=1 {
            if round > 0 {
                optimize(&c, f, 1);
            }
            for first_poison in [false, true] {
                for last_poison in [false, true] {
                    let mut state = ExecutionState::default();
                    let p = state.memory.alloc(16, 1);
                    state.memory.write(p, &[0xa5; 16]).unwrap();
                    let first = if first_poison {
                        Value::Poison
                    } else {
                        Value::Int(APInt::new(width, 0x123456789abcdef0))
                    };
                    let last = if last_poison {
                        Value::Poison
                    } else {
                        Value::Int(APInt::new(width, 0x8765432101234567))
                    };
                    let result = interp::run_function_in_state(
                        &c,
                        f,
                        vec![Value::Ptr(p + 1), first, last],
                        &mut state,
                        Some(100),
                    )
                    .unwrap();
                    let bytes = width / 8;
                    if last_poison {
                        assert!(matches!(&result[..], [Value::Poison]));
                        assert!(matches!(
                            state.memory.read(p + 1, bytes),
                            Err(interp::InterpError::Uninitialized { .. })
                        ));
                    } else {
                        assert!(
                            matches!(&result[..], [Value::Int(v)] if *v == APInt::new(width, 0x8765432101234567))
                        );
                        assert_eq!(
                            state.memory.read(p + 1, bytes).unwrap(),
                            &0x8765432101234567u64.to_le_bytes()[..bytes as usize]
                        );
                    }
                    assert_eq!(state.memory.read(p, 1).unwrap(), [0xa5]);
                    assert_eq!(
                        state.memory.read(p + 1 + bytes, 15 - bytes).unwrap(),
                        vec![0xa5; (15 - bytes) as usize]
                    );
                }
            }
            for address in [Value::Ptr(0), Value::Ptr(u64::MAX - 16), Value::Poison] {
                let mut state = ExecutionState::default();
                let p = state.memory.alloc(8, 1);
                state.memory.write(p, &[0xa5; 8]).unwrap();
                let result = interp::run_function_in_state(
                    &c,
                    f,
                    vec![
                        address,
                        Value::Int(APInt::new(width, 1)),
                        Value::Int(APInt::new(width, 2)),
                    ],
                    &mut state,
                    Some(100),
                );
                assert!(matches!(
                    result,
                    Err(interp::InterpError::OutOfBounds { .. } | interp::InterpError::Undefined(_))
                ));
                assert_eq!(state.memory.read(p, 8).unwrap(), [0xa5; 8]);
            }
        }
    }
}

#[test]
fn implicit_overwrites_keep_partial_producers_and_intervening_traps() {
    let producer = STORES.replace(
        "    ptr.store %x",
        "    %z = constant {value = 0} : !i32\n    %d = divsi %x, %z : !i32\n    ptr.store %d",
    );
    let intervening = STORES.replace(
        "    ptr.store %x, %p",
        "    %z = constant {value = 0} : !i32\n    ptr.store %x, %p\n    %d = divsi %x, %z : !i32",
    );
    let fp = STORES.replace("%y: !i32)", "%y: !i32, %a: !f64, %b: !f64)")
        .replace("    ptr.store %y", "    %q, %env1, %mem1 = fp.div %a, %b state(%env, %mem) {semantics = {exceptions = \"flags_and_traps\"}} : !f64\n    ptr.store %y");
    let fp = fp.replace("    ptr.store %x", "    %env = state.entry_state : !state<fp.env>\n    %mem = state.entry_state : !state<memory>\n    ptr.store %x");
    let foreign = STORES
        .replace("%y: !i32)", "%y: !i32, %bad: !ptr.p)")
        .replace(
            "    ptr.store %y",
            "    %unread = ptr.load %bad : !i32\n    ptr.store %y",
        );
    for (case, source) in [producer, intervening, fp, foreign].iter().enumerate() {
        let (c, f) = implicit_fixture(source);
        for round in 0..=1 {
            if round > 0 {
                let output = optimize(&c, f, if case == 0 { 1 } else { 2 });
                assert!(
                    output.contains(match case {
                        2 => "fp.div",
                        3 => "ptr.load",
                        _ => "divsi",
                    }),
                    "{output}"
                );
            }
            let mut state = ExecutionState::default();
            let p = state.memory.alloc(8, 1);
            state.memory.write(p, &[0xa5; 8]).unwrap();
            state.fp_environment.traps = 31;
            let mut args = vec![
                Value::Ptr(p),
                Value::Int(APInt::new(32, 0x12345678)),
                Value::Int(APInt::new(32, 0x87654321)),
            ];
            if case == 2 {
                args.extend([1.0f64, 0.0f64].map(|value| {
                    Value::Float(tir::utils::APFloat::from_bits(
                        11,
                        52,
                        false,
                        value.to_bits().into(),
                    ))
                }));
            }
            if case == 3 {
                args.push(Value::Ptr(0));
            }
            let result = interp::run_function_in_state(&c, f, args, &mut state, Some(100));
            if case == 2 {
                assert!(matches!(result, Err(interp::InterpError::Message(_))));
                assert_eq!(state.fp_environment.flags, 8);
            } else if case == 3 {
                assert!(matches!(
                    result,
                    Err(interp::InterpError::OutOfBounds { .. })
                ));
            } else {
                assert!(matches!(result, Err(interp::InterpError::Undefined(_))));
            }
            assert_eq!(
                state.memory.read(p, 4).unwrap(),
                if case == 0 {
                    [0xa5; 4]
                } else {
                    [0x78, 0x56, 0x34, 0x12]
                }
            );
            assert_eq!(state.memory.read(p + 4, 4).unwrap(), [0xa5; 4]);
        }
    }
}

#[test]
fn implicit_overwrites_require_unobserved_ordinary_same_pointer_full_width_stores() {
    let mut controls = vec![
        STORES.replace("ptr.store %x, %p", "ptr.store %x, %p\n    %z = constant {value = 123, keep = true} : !i32"),
        STORES.replace("ptr.store %x, %p", "ptr.store %x, %p\n    %w = ptr.load %p {volatile = true} : !i32"),
        STORES.replace("ptr.store %x, %p", "ptr.store %x, %p\n    cfg.br ^next\n  ^next:"),
        STORES.replace("%y: !i32)", "%y: !i32, %q: !ptr.p)").replace("ptr.store %y, %p", "ptr.store %y, %q"),
        STORES.replace("ptr.store %x, %p", "%small = trunci %x : !i16\n    ptr.store %small, %p"),
        STORES.replace("!i32", "!i128"),
        STORES.replace("module {", "module {layout.spec = #layout.spec<!ptr.p = {size = 32, abi = 32}>} {"),
        STORES.replace("module {", "module {layout.spec = #layout.spec<endianness = \"big\", !ptr.p = {size = 64, abi = 64}>} {"),
        STORES.replace("ptr.store %x, %p", "ptr.store %x, %p\n    func.call %g()")
            .replace("  module_end", "  %g = func.declare @g() -> !unit\n  module_end"),
    ];
    for store in ["ptr.store %x, %p", "ptr.store %y, %p"] {
        for attribute in ["volatile", "atomic", "keep"] {
            controls.push(STORES.replace(store, &format!("{store} {{{attribute} = true}}")));
        }
    }
    for source in controls {
        let (c, f) = implicit_fixture(&source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        optimize(&c, f, 2);
    }
}

#[test]
fn noalias_parameter_stores_preserve_faults_and_prior_external_bytes() {
    let source =
        include_str!("../../../../../core/checks/Inputs/eqsat/dse_noalias_parameter_dies.tir");
    let (c, f) = implicit_fixture(source);
    for round in 0..=1 {
        if round > 0 {
            let output = optimize(&c, f, 3);
            filecheck::verify(
                &filecheck::Source::new("noalias store barrier", source),
                &filecheck::Source::new("optimized", &output),
                &filecheck::Config::default(),
            )
            .unwrap();
        }
        for poison in [false, true] {
            let mut state = ExecutionState::default();
            let p = state.memory.alloc(4, 1);
            let q = state.memory.alloc(4, 1);
            state.memory.write(p, &[0xa5; 4]).unwrap();
            state.memory.write(q, &[0x5a; 4]).unwrap();
            let result = interp::run_function_in_state(
                &c,
                f,
                vec![
                    Value::Ptr(p),
                    Value::Ptr(q + 4),
                    if poison {
                        Value::Poison
                    } else {
                        Value::Int(APInt::new(32, 0x12345678))
                    },
                    Value::Int(APInt::new(32, 0x87654321)),
                ],
                &mut state,
                Some(100),
            );
            assert!(
                matches!(result, Err(interp::InterpError::OutOfBounds { address, size: 4 }) if address == q + 4)
            );
            if poison {
                assert!(matches!(
                    state.memory.read(p, 4),
                    Err(interp::InterpError::Uninitialized { .. })
                ));
            } else {
                assert_eq!(
                    state.memory.read(p, 4).unwrap(),
                    0x12345678u32.to_le_bytes()
                );
            }
            assert_eq!(state.memory.read(q, 4).unwrap(), [0x5a; 4]);
        }
    }
}
