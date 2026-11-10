use super::*;
use tir::{parse::ir::parse_ir, AnalysisManager, Pass};

const OFFSETS: &str =
    include_str!("../../../../../core/checks/Inputs/eqsat/store_forward_ptradd_offsets.tir");

#[test]
fn pointer_addresses_forward_equal_offsets_and_keep_overlapping_reads() {
    for wrong_offset in [false, true] {
        let source = if wrong_offset {
            OFFSETS.replace("%s = ptr.ptradd %p, %four", "%s = ptr.ptradd %p, %two")
        } else {
            OFFSETS.to_owned()
        };
        let c = Context::with_default_dialects();
        let module = parse_ir(&c, &source).unwrap();
        let functions = c.get_region(module.regions()[0]).op_ids();
        tir::verify_op_tree(&c, module.id()).unwrap();
        for (index, &f) in functions[..2].iter().enumerate() {
            let body = c.get_region(c.get_op(f).regions()[0]);
            let ops = body.iter(c.clone()).next().unwrap().op_ids();
            let load = *ops
                .iter()
                .find(|&&op| c.get_op(op).is_operation(&tir::ptr::LOAD))
                .unwrap();
            let store = *ops
                .iter()
                .find(|&&op| c.get_op(op).is_operation(&tir::ptr::STORE))
                .unwrap();
            let same = index == 0 && !wrong_offset;
            for optimized in [false, true] {
                if optimized {
                    tir::passes::EqSatPass::new()
                        .run(f, &c, &AnalysisManager::new())
                        .unwrap();
                    tir::verify_op_tree(&c, module.id()).unwrap();
                    assert_eq!(
                        c.has_operation(load),
                        !same,
                        "wrong_offset={wrong_offset} function={index}\n{}",
                        printed_memory(&c, f)
                    );
                    assert!(c.has_operation(store));
                }
                for bits in [0u32, 0x12345678, u32::MAX] {
                    let mut state = ExecutionState::default();
                    let base = state.memory.alloc(12, 1);
                    state.memory.write(base, &[0xa5; 12]).unwrap();
                    let actual = interp::run_function_in_state(
                        &c,
                        f,
                        vec![Value::Ptr(base), Value::Int(APInt::new(32, bits as u64))],
                        &mut state,
                        None,
                    )
                    .unwrap();
                    let expected = if same { bits } else { (bits << 16) | 0xa5a5 };
                    assert!(
                        matches!(&actual[..], [Value::Int(value)] if value.to_u64() == expected as u64),
                        "{actual:?}"
                    );
                    let mut memory = [0xa5; 12];
                    memory[4..8].copy_from_slice(&bits.to_le_bytes());
                    assert_eq!(state.memory.read(base, 12).unwrap(), memory);
                }
            }
        }
        if !wrong_offset {
            filecheck::verify(
                &filecheck::Source::new("pointer offsets", OFFSETS),
                &filecheck::Source::new("optimized", printed_memory(&c, functions[0])),
                &filecheck::Config::default(),
            )
            .unwrap();
        }
    }
}

#[test]
fn pointer_address_identities_preserve_wrapping_and_poison() {
    let zero = "%zero = constant {value = 0} : !i64\n%q = ptr.ptradd %p, %zero : !ptr.p";
    let wrapping = "%minus = constant {value = -1} : !i64\n%one = constant {value = 1} : !i64\n%r = ptr.ptradd %p, %minus : !ptr.p\n%q = ptr.ptradd %r, %one : !ptr.p";
    for body in [zero, wrapping] {
        for graph in [false, true] {
            let terminator = if graph { "-> %q" } else { "func.return %q" };
            let source = format!(
                "module {{\n%f = func.func @identity(%p: !ptr.p) -> !ptr.p {{\n{body}\n{terminator}\n}}\nmodule_end\n}}"
            );
            let c = Context::with_default_dialects();
            let module = parse_ir(&c, &source).unwrap();
            let f = c.get_region(module.regions()[0]).op_ids()[0];
            tir::verify_op_tree(&c, module.id()).unwrap();
            for optimized in [false, true] {
                if optimized {
                    tir::passes::EqSatPass::new()
                        .run(f, &c, &AnalysisManager::new())
                        .unwrap();
                    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
                    assert!(!printed_memory(&c, f).contains("ptr.ptradd"));
                }
                for pointer in [
                    Value::Ptr(0),
                    Value::Ptr(7),
                    Value::Ptr(u64::MAX),
                    Value::Poison,
                ] {
                    let actual = interp::run_function(&c, f, vec![pointer.clone()]).unwrap();
                    match (actual.as_slice(), pointer) {
                        ([Value::Ptr(actual)], Value::Ptr(expected)) => {
                            assert_eq!(*actual, expected)
                        }
                        ([Value::Poison], Value::Poison) => {}
                        _ => panic!("unexpected pointer identity {actual:?}"),
                    }
                }
            }
        }
    }
}

#[test]
fn pointer_addresses_keep_unsupported_metadata_width_layout_and_payload() {
    let identity = "module {\n%f = func.func @identity(%p: !ptr.p) -> !ptr.p {\n%zero = constant {value = 0} : !i64\n%q = ptr.ptradd %p, %zero : !ptr.p\nfunc.return %q\n}\nmodule_end\n}";
    for source in [
        identity.replace("%zero : !ptr.p", "%zero {tag = 1} : !ptr.p"),
        identity.replace("!i64", "!i32"),
        identity.replace("!i64", "!i32").replace(
            "module {",
            "module {layout.spec = #layout.spec<!ptr.p = {size = 32, abi = 32}>} {",
        ),
        identity
            .replace("%p: !ptr.p", "%p: !ptr.p<!i32>")
            .replace("-> !ptr.p", "-> !ptr.p<!i8>")
            .replace("%zero : !ptr.p", "%zero : !ptr.p<!i8>"),
    ] {
        let (c, f, _, _) = fixture(&source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        assert!(printed_memory(&c, f).contains("ptr.ptradd"), "{source}");
    }
    let source = OFFSETS.replace("#layout.spec<", "#layout.spec<endianness = \"big\", ");
    let (c, f, _, _) = fixture(&source);
    let body = c.get_region(c.get_op(f).regions()[0]);
    let load = *body
        .iter(c.clone())
        .next()
        .unwrap()
        .op_ids()
        .iter()
        .find(|&&op| c.get_op(op).is_operation(&tir::ptr::LOAD))
        .unwrap();
    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
    tir::passes::EqSatPass::new()
        .run(f, &c, &AnalysisManager::new())
        .unwrap();
    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
    assert!(c.has_operation(load));
}
