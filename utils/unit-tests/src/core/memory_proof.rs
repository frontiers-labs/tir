use tir::builtin::MODULE;
use tir::interp::{self, ExecutionState, Value};
use tir::sem::memory::check_forward;
use tir::utils::APInt;
use tir::{Context, OpId, Operation};

const SOURCE: &str = r#"module {layout.spec = #layout.spec<endianness = "little", !ptr.p = {size = 64, abi = 64}>} {
  %f = func.func @f(%x: !i32, %p: !ptr.p<!i32>) -> !i32 {
    %entry = state.entry_state : !state<memory>
    %stored = ptr.store %x, %p state(%entry)
    %v, %loaded = ptr.load %p state(%stored) : !i32
    func.return %v
  }
  module_end
}"#;

fn fixture(source: &str) -> (Context, OpId, OpId, OpId) {
    let c = Context::with_default_dialects();
    let m = tir::parse::ir::parse_operation(&c, source, &tir::builtin::MODULE).unwrap();
    let f = c.get_region(m.regions()[0]).op_ids()[0];
    let region = c.get_region(c.get_op(f).regions()[0]);
    let ops = region
        .iter(c.clone())
        .next()
        .map_or_else(|| region.op_ids(), |block| block.op_ids());
    (c, f, ops[1], ops[2])
}

#[test]
fn ordinary_i32_forwarding_checks_actual_declarations_and_complete_mapping() {
    let (c, f, s, l) = fixture(SOURCE);
    let w = check_forward(&c, f, s, l).unwrap();
    w.validate(&c).unwrap();
    let prerequisite = c
        .get_value(c.get_op(s).state_operands()[0])
        .defining_op()
        .unwrap();
    for source_id in [f, prerequisite] {
        let attributes = c.get_op(source_id).attributes();
        let mut changed = attributes.clone();
        changed.push(c.named_attribute("proof_snapshot_control", true.into()));
        c.set_op_attributes(source_id, changed);
        assert_eq!(
            w.validate(&c),
            Err(tir::sem::memory::MemoryProofError::Stale)
        );
        c.set_op_attributes(source_id, attributes);
        w.validate(&c).unwrap();
    }
    let attributes = c.get_op(s).attributes();
    let mut changed = attributes.clone();
    changed.push(c.named_attribute("volatile", true.into()));
    c.set_op_attributes(s, changed);
    assert!(w.validate(&c).is_err());
    c.set_op_attributes(s, attributes);
    w.validate(&c).unwrap();
    assert_eq!(w.retained_store(), s);
    assert_eq!(
        w.replacements(),
        &[
            (
                c.get_op(l).value_results()[0],
                c.get_op(s).value_operands()[0]
            ),
            (
                c.get_op(l).state_results()[0],
                c.get_op(s).state_results()[0]
            )
        ]
    );

    let module = c.parent_op(f).unwrap();
    let attributes = c.get_op(module).attributes();
    c.set_op_attributes(module, Vec::new());
    assert_eq!(
        w.validate(&c),
        Err(tir::sem::memory::MemoryProofError::Stale)
    );
    let default = check_forward(&c, f, s, l).unwrap();
    default.validate(&c).unwrap();
    c.set_op_attributes(module, attributes);
    assert_eq!(
        default.validate(&c),
        Err(tir::sem::memory::MemoryProofError::Stale)
    );
    w.validate(&c).unwrap();

    let unused = tir::parse::ir::parse_op(&c, "constant {value = 9} : !i32").unwrap();
    c.insert(c.insertion_point(s), unused.id());
    let w = check_forward(&c, f, s, l).unwrap();
    let placement = c.insertion_point(s);
    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
    c.insert(c.insertion_point(unused.id()), s);
    assert_ne!(c.insertion_point(s), placement);
    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
    assert_eq!(
        w.validate(&c),
        Err(tir::sem::memory::MemoryProofError::Stale)
    );
    c.insert(c.insertion_point(l), s);
    w.validate(&c).unwrap();
}

#[test]
fn ordinary_i32_forwarding_requires_compatible_domain() {
    for source in [
        SOURCE.replace("!i32", "!i1"),
        SOURCE.replace("!i32", "!i128"),
        SOURCE.replace("ptr.p<!i32>", "ptr.p<!i64>"),
        SOURCE.replace("state(%stored)", "state(%entry)"),
        SOURCE
            .replace("%p: !ptr.p<!i32>", "%p: !ptr.p<!i32>, %q: !ptr.p<!i32>")
            .replace("ptr.load %p", "ptr.load %q"),
        SOURCE.replace(
            "ptr.load %p state(%stored) : !i32",
            "ptr.load %p state(%stored) : !i16",
        ),
        SOURCE.replace("size = 64, abi = 64", "size = 32, abi = 32"),
        SOURCE.replace("little", "big"),
        SOURCE.replace(", !ptr.p = {size = 64, abi = 64}", ""),
        SOURCE.replace("%v, %loaded =", "%v, %loaded, %extra ="),
    ] {
        let (c, f, s, l) = fixture(&source);
        assert!(check_forward(&c, f, s, l).is_err(), "{source}");
    }
    let (c, f, s, l) = fixture(SOURCE);
    assert!(check_forward(&c, c.parent_op(f).unwrap(), s, l).is_err());
    let extra_function = r#"  %g = func.func @g(%x: !i32, %p: !ptr.p<!i32>) -> !i32 {
        %s = state.entry_state : !state<memory>
        %w = ptr.store %x, %p state(%s)
        %v, %r = ptr.load %p state(%w) : !i32
        func.return %v
    }
"#;
    let (cross, owner, store, _) =
        fixture(&SOURCE.replace("  module_end", &format!("{extra_function}  module_end")));
    let module = cross.parent_op(owner).unwrap();
    let other = cross.get_region(cross.get_op(module).regions()[0]).op_ids()[1];
    let other_body = cross.get_region(cross.get_op(other).regions()[0]);
    let other_load = other_body.iter(cross.clone()).next().unwrap().op_ids()[2];
    assert!(check_forward(&cross, owner, store, other_load).is_err());
    for attribute in ["volatile", "atomic", "unknown_contract"] {
        let attrs = c.get_op(l).attributes();
        let mut changed = attrs.clone();
        changed.push(c.named_attribute(attribute, true.into()));
        c.set_op_attributes(l, changed);
        assert!(check_forward(&c, f, s, l).is_err());
        c.set_op_attributes(l, attrs);
    }
}

#[test]
fn ordinary_i32_retained_store_execution_matches_bits_poison_failure_and_cells() {
    let (before, f, _, _) = fixture(SOURCE);
    let replacement = SOURCE
        .replace("    %v, %loaded = ptr.load %p state(%stored) : !i32\n", "")
        .replace("func.return %v", "func.return %x");
    let (after, g, _, terminal) = fixture(&replacement);
    assert!(after.get_op(terminal).is_operation(&tir::func::RETURN));
    for bits in [0, u32::MAX as u64, 0x12345678] {
        for initialized in [false, true] {
            for poison in [false, true] {
                for offset in [0, 1, 4, 5, 8] {
                    retained_store_case(
                        (&before, f),
                        (&after, g),
                        bits,
                        initialized,
                        poison,
                        offset,
                    );
                }
            }
        }
    }
    let load_source = SOURCE
        .replace("    %stored = ptr.store %x, %p state(%entry)\n", "")
        .replace("state(%stored)", "state(%entry)");
    let (c, load, _, _) = fixture(&load_source);
    for initialized in [false, true] {
        let mut state = ExecutionState::default();
        let base = state.memory.alloc(4, 1);
        if initialized {
            state.memory.write(base, &[0x78, 0x56, 0x34, 0x12]).unwrap();
        }
        let memory_before = format!("{:?}", state.memory.read(base, 4));
        let value = interp::run_function_in_state(
            &c,
            load,
            vec![Value::Int(APInt::new(32, 0)), Value::Ptr(base)],
            &mut state,
            None,
        )
        .unwrap();
        match (&value[..], initialized) {
            ([Value::Poison], false) => {}
            ([Value::Int(v)], true) => assert_eq!(v.to_u64(), 0x12345678),
            _ => panic!("unexpected virgin load outcome {value:?}"),
        }
        assert_eq!(format!("{:?}", state.memory.read(base, 4)), memory_before);
    }
    for address in [Value::Ptr(0), Value::Ptr(u64::MAX - 8), Value::Poison] {
        let args = vec![Value::Int(APInt::new(32, 1)), address];
        let mut a = ExecutionState::default();
        let mut b = ExecutionState::default();
        let av = interp::run_function_in_state(&before, f, args.clone(), &mut a, None);
        let bv = interp::run_function_in_state(&after, g, args, &mut b, None);
        assert!(av.is_err());
        assert_eq!(format!("{av:?}"), format!("{bv:?}"));
    }
}

fn retained_store_case(
    (before, f): (&Context, OpId),
    (after, g): (&Context, OpId),
    bits: u64,
    initialized: bool,
    poison: bool,
    offset: u64,
) {
    let mut a = ExecutionState::default();
    let mut b = ExecutionState::default();
    let base = a.memory.alloc(8, 1);
    assert_eq!(base, b.memory.alloc(8, 1));
    if initialized {
        a.memory.write(base, &[0xa5; 8]).unwrap();
        b.memory.write(base, &[0xa5; 8]).unwrap();
    }
    let x = if poison {
        Value::Poison
    } else {
        Value::Int(APInt::new(32, bits))
    };
    let args = vec![x, Value::Ptr(base + offset)];
    let av = interp::run_function_in_state(before, f, args.clone(), &mut a, None);
    let bv = interp::run_function_in_state(after, g, args, &mut b, None);
    eprintln!(
        "memory observation bits={bits} init={initialized} poison={poison} offset={offset} before={av:?} after={bv:?} bytes-before={:?} bytes-after={:?}",
        a.memory.read(base, 8),
        b.memory.read(base, 8)
    );
    assert_eq!(format!("{av:?}"), format!("{bv:?}"));
    for i in 0..8 {
        assert_eq!(
            format!("{:?}", a.memory.read(base + i, 1)),
            format!("{:?}", b.memory.read(base + i, 1))
        );
        let modified = offset <= 4 && (offset..offset + 4).contains(&i);
        let expected = if modified && !poison {
            Some(((bits >> ((i - offset) * 8)) & 255) as u8)
        } else if !modified && initialized {
            Some(0xa5)
        } else {
            None
        };
        match expected {
            Some(byte) => assert_eq!(a.memory.read(base + i, 1).unwrap(), [byte]),
            None => assert!(matches!(
                a.memory.read(base + i, 1),
                Err(interp::InterpError::Uninitialized { .. })
            )),
        }
    }
    if offset <= 4 && !poison {
        assert_eq!(
            a.memory.read(base + offset, 4).unwrap(),
            (bits as u32).to_le_bytes()
        );
    }
    if offset > 4 && initialized {
        assert_eq!(a.memory.read(base, 8).unwrap(), [0xa5; 8]);
    }
}

#[test]
fn checked_patterns_request_engine_captures_without_authorizing_rewrites() {
    use tir_pdl::CaptureRole;
    const RULE: &str = "rule checked-forward: ptr.load(p: ptr<P> | ptr.store(x: int<32>, p | s: state<memory>)) => x proof smt;";
    fn requests(c: &Context, program: &tir_pdl::CompiledEgglog) -> Vec<Vec<i64>> {
        let mut engine = c.egraph_with_rules().unwrap();
        let source = format!(
            "{}\n(run-schedule (saturate ir-index) (saturate Checkedrequest-types) (saturate Checkedrequests))",
            program.source
        );
        eprintln!("generated requests:\n{source}");
        let commands = engine
            .parse_program("compiled checked pattern", &source)
            .unwrap();
        engine.run_program(commands).unwrap();
        let db = engine.database();
        let rows = db
            .rows(db.table(&program.obligations[0].relation).unwrap())
            .map(|row| row.values().map(|v| db.base_value::<i64>(v)).collect())
            .collect();
        eprintln!("request rows: {rows:?}");
        rows
    }
    fn printed(c: &Context, f: OpId) -> String {
        let m = c
            .get_op(c.parent_op(f).unwrap())
            .as_operation(&MODULE)
            .unwrap();
        let mut text = String::new();
        tir::print_ir(&m, c, &mut tir::IRFormatter::new(&mut text)).unwrap();
        text
    }
    let library = tir_pdl::compile(RULE).unwrap();
    let program = tir_pdl::emit_egglog(&library, "Checked").unwrap();
    assert_eq!(program.obligations.len(), 1);
    let obligation = &program.obligations[0];
    assert_eq!(obligation.rule, "checked-forward");
    let (c, f, store, load) = fixture(SOURCE);
    let before = printed(&c, f);
    let rows = requests(&c, &program);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let capture = |name: &str| {
        row[obligation
            .captures
            .iter()
            .position(|c| c.name == name)
            .unwrap()]
    };
    assert_eq!(row[obligation.root], i64::from(load.number()));
    let nested = obligation
        .captures
        .iter()
        .enumerate()
        .find(|(i, c)| *i != obligation.root && c.role == CaptureRole::Operation)
        .unwrap()
        .0;
    assert_eq!(row[nested], i64::from(store.number()));
    assert_eq!(
        capture("p"),
        i64::from(c.get_op(store).value_operands()[1].number())
    );
    assert_eq!(
        capture("x"),
        i64::from(c.get_op(store).value_operands()[0].number())
    );
    assert_eq!(
        capture("s"),
        i64::from(c.get_op(store).state_operands()[0].number())
    );
    let id = |n: i64| OpId::from_number(n.try_into().unwrap());
    // Context proof reads run after the engine guard in requests has ended.
    let witness = check_forward(&c, f, id(row[nested]), id(row[obligation.root])).unwrap();
    witness.validate(&c).unwrap();
    assert_eq!(witness.retained_store(), store);
    assert_eq!(
        witness.replacements(),
        &[
            (
                c.get_op(load).value_results()[0],
                c.get_op(store).value_operands()[0]
            ),
            (
                c.get_op(load).state_results()[0],
                c.get_op(store).state_results()[0]
            ),
        ]
    );
    let (old, new) = obligation.requested_mapping;
    assert_eq!(
        (row[old], row[new]),
        (
            i64::from(witness.replacements()[0].0.number()),
            i64::from(witness.replacements()[0].1.number()),
        )
    );
    eprintln!(
        "checker mapping: {:?}; retained store: {store:?}",
        witness.replacements()
    );
    assert!(tir::passes::EqSatPass::with_library(library).is_err());
    assert_eq!(printed(&c, f), before);

    for source in [
        SOURCE.replace("!i32", "!i64"),
        SOURCE
            .replace("%p: !ptr.p<!i32>", "%p: !ptr.p<!i32>, %q: !ptr.p<!i32>")
            .replace("ptr.load %p", "ptr.load %q"),
        SOURCE.replace("state(%stored)", "state(%entry)"),
        SOURCE.replace(
            "ptr.load %p state(%stored) : !i32",
            "ptr.load %p state(%stored) : !i16",
        ),
    ] {
        let (c, _, _, _) = fixture(&source);
        assert!(requests(&c, &program).is_empty(), "{source}");
    }
    for pattern in [
        "ptr.load(p: ptr<P>, ptr.store(x: int<32>, p | s: state<memory>))",
        "ptr.load(p: ptr<P> | ptr.store(x: int<32> | s: state<memory>))",
        "ptr.load(p: int<32> | ptr.store(x: int<32>, p | s: state<memory>))",
    ] {
        let library =
            tir_pdl::compile(&format!("rule different: {pattern} => x proof smt;")).unwrap();
        let (c, _, _, _) = fixture(SOURCE);
        let program = tir_pdl::emit_egglog(&library, "Checked").unwrap();
        assert!(requests(&c, &program).is_empty(), "{pattern}");
    }
    for source in [
        RULE.replace("int<32>", "int<P>"),
        RULE.replace("=> x", "=> s"),
        RULE.replace("=> x", "=> ptr.load(p | s)"),
        RULE.replace("=> x", "=> 0"),
        RULE.replace("ptr.load(", "ptr.load<volatile = 1>("),
        RULE.replace("proof smt", "where defined(x) proof smt"),
        RULE.replace("proof smt", "proof definitional"),
        RULE.replace("=>", "<=>"),
    ] {
        let library = tir_pdl::compile(&source).unwrap();
        assert!(
            tir_pdl::emit_egglog(&library, "Checked").is_err(),
            "{source}"
        );
    }
    let (c, f, _, _) = fixture(&SOURCE.replace(
        "    func.return",
        "    %twice = addi %x, %x : !i32\n    func.return",
    ));
    let body = c.get_region(c.get_op(f).regions()[0]);
    let twice = body.iter(c.clone()).next().unwrap().op_ids()[3];
    let program = tir_pdl::emit_egglog(
        &tir_pdl::compile("rule other-vocabulary: builtin.addi(x: int<32>, x) => x proof smt;")
            .unwrap(),
        "Checked",
    )
    .unwrap();
    let rows = requests(&c, &program);
    assert_eq!(rows.len(), 1);
    assert_eq!(program.obligations[0].rule, "other-vocabulary");
    assert_eq!(
        rows[0][program.obligations[0].root],
        i64::from(twice.number())
    );
}

#[test]
fn checked_forwarding_publishes_through_default_and_bound_pass() {
    use tir::{AnalysisManager, Pass};
    for mut pass in composition_passes() {
        for unordered in [false, true] {
            let source = SOURCE.replace("    func.return %v", "    %peer = muli %x, %x : !i32\n    %observed, %last = ptr.load %p state(%loaded) : !i32\n    %sum = addi %v, %observed : !i32\n    %out = addi %sum, %peer : !i32\n    func.return %out");
            let source = if unordered {
                source.replace("func.return %out", "-> %out")
            } else {
                source
            };
            let (before, f, _, _) = fixture(&source);
            let (after, g, store, load) = fixture(&source);
            let scope = after.parent_op(g).unwrap();
            let peer = after.get_op(g).regions()[0];
            let original = after.get_region(peer).op_ids();
            let value_consumer = original[5];
            let observable_peer = original[3];
            let prerequisite = original[0];
            pass.run(g, &after, &AnalysisManager::new()).unwrap();
            tir::verify_op_tree(&after, scope).unwrap();
            assert!(!after.has_operation(load) || !after.get_op(load).is_live());
            assert!(after.get_op(store).is_live());
            let operations = after.get_region(peer).op_ids();
            assert_eq!(operations.len(), 6);
            for retained in [store, observable_peer, prerequisite] {
                assert!(operations.contains(&retained));
            }
            assert_eq!(
                after.get_op(value_consumer).value_operands()[0],
                after.get_op(store).value_operands()[0]
            );
            assert!(operations
                .iter()
                .any(|&op| after.get_op(op).name().as_str() == "addi"));
            let remaining_loads = operations
                .iter()
                .copied()
                .filter(|&op| after.get_op(op).name().as_str() == "load")
                .collect::<Vec<_>>();
            assert_eq!(remaining_loads.len(), usize::from(unordered));
            if let Some(&state_consumer) = remaining_loads.first() {
                assert_eq!(
                    after.get_op(state_consumer).state_operands(),
                    after.get_op(store).state_results()
                );
            }
            let mut printed = String::new();
            tir::print_ir(
                &after.get_op(g).as_operation(&tir::func::FUNC).unwrap(),
                &after,
                &mut tir::IRFormatter::new(&mut printed),
            )
            .unwrap();
            eprintln!("published root: {printed}");
            for bits in [0, u32::MAX as u64, 0x12345678] {
                for init in [false, true] {
                    for poison in [false, true] {
                        for offset in [0, 1, 4, 5, 8] {
                            retained_store_case(
                                (&before, f),
                                (&after, g),
                                bits,
                                init,
                                poison,
                                offset,
                            );
                        }
                    }
                }
            }
            for address in [Value::Ptr(0), Value::Ptr(u64::MAX - 8), Value::Poison] {
                let args = vec![Value::Int(APInt::new(32, 1)), address];
                let mut a = ExecutionState::default();
                let mut b = ExecutionState::default();
                let av = interp::run_function_in_state(&before, f, args.clone(), &mut a, None);
                let bv = interp::run_function_in_state(&after, g, args, &mut b, None);
                assert!(av.is_err());
                assert_eq!(format!("{av:?}"), format!("{bv:?}"));
                eprintln!("published pointer failure comparison: {av:?} == {bv:?}");
            }
            pass.run(g, &after, &AnalysisManager::new()).unwrap();
            tir::verify_op_tree(&after, scope).unwrap();
        }
    }
}

#[test]
fn checked_default_forwarding_removes_only_the_covered_load() {
    use tir::Pass;
    let originals = [
        (
            include_str!("../../../../core/checks/Inputs/eqsat/store_forward.tir"),
            false,
        ),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/store_forward_constant.tir"),
            true,
        ),
        (SOURCE, false),
    ];
    for (source, constant) in originals {
        let (before, f, _, _) = fixture(source);
        let (c, g, _, _) = fixture(source);
        let region = c.get_op(g).regions()[0];
        let ops = c.get_region(region).op_ids();
        let store = *ops
            .iter()
            .find(|&&op| c.get_op(op).is_operation(&tir::ptr::STORE))
            .unwrap();
        let load = *ops
            .iter()
            .find(|&&op| c.get_op(op).is_operation(&tir::ptr::LOAD))
            .unwrap();
        let ret = *ops.last().unwrap();
        let value = c.get_op(store).value_operands()[0];
        tir::verify_op_tree(&c, c.parent_op(g).unwrap()).unwrap();
        tir::passes::EqSatPass::new()
            .run(g, &c, &tir::AnalysisManager::new())
            .unwrap();
        eprintln!(
            "basic forwarding constant={constant}\n{}",
            printed_memory(&c, g)
        );
        assert!(
            !c.has_operation(load),
            "covered load must be physically removed"
        );
        assert_eq!(
            c.get_region(region).op_ids(),
            ops.into_iter().filter(|&op| op != load).collect::<Vec<_>>()
        );
        assert!(c.get_op(store).is_live());
        assert_eq!(c.get_op(ret).value_operands().as_slice(), [value]);
        tir::verify_op_tree(&c, c.parent_op(g).unwrap()).unwrap();
        composed_execution(
            (&before, f),
            (&c, g),
            |bits| {
                let value = if constant { 7 } else { bits };
                (value, value)
            },
            constant,
        );
    }
}

#[test]
fn checked_publication_preserves_unsupported_sources_and_scopes() {
    use tir::passes::eqsat::{EqSatLibrary, EqSatPass};
    use tir::Pass;
    const RULE: &str = "rule caller-forward: ptr.load(p: ptr<P> | ptr.store(x: int<32>, p | s: state<memory>)) => x proof smt;";
    assert!(EqSatPass::with_library(tir_pdl::compile(RULE).unwrap()).is_err());
    let mut no_proof = EqSatPass::with_library(
        EqSatLibrary::new(tir_pdl::compile(RULE).unwrap()).bind("caller-forward", |_, _, _| None),
    )
    .unwrap();
    let (c, f, _, load) = fixture(SOURCE);
    no_proof.run(f, &c, &tir::AnalysisManager::new()).unwrap();
    assert!(c.get_op(load).is_live());
    let mut pass = EqSatPass::new();
    for source in [
        SOURCE
            .replace("%p: !ptr.p<!i32>", "%p: !ptr.p<!i32>, %q: !ptr.p<!i32>")
            .replace("ptr.load %p", "ptr.load %q"),
        SOURCE.replace("ptr.p<!i32>", "ptr.p<!i64>"),
        SOURCE.replace(", !ptr.p = {size = 64, abi = 64}", ""),
        SOURCE.replace("%v, %loaded =", "%v, %loaded, %extra ="),
        SOURCE
            .replace("%v, %loaded =", "%v =")
            .replace("func.return %v", "-> %v"),
        SOURCE.replace("!i32", "!i128"),
        SOURCE.replace("little", "big"),
        SOURCE.replace("size = 64, abi = 64", "size = 32, abi = 32"),
        SOURCE.replace("state(%stored)", "state(%entry)"),
        SOURCE.replace("func.return %v", "-> %v"),
    ] {
        let (c, f, _, load) = fixture(&source);
        let result = pass.run(f, &c, &tir::AnalysisManager::new());
        assert!(c.get_op(load).is_live(), "{source}");
        if result.is_ok() {
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        }
    }
    for attribute in ["volatile", "atomic"] {
        let (c, f, _, load) = fixture(SOURCE);
        let mut attributes = c.get_op(load).attributes();
        attributes.push(c.named_attribute(attribute, true.into()));
        c.set_op_attributes(load, attributes);
        pass.run(f, &c, &tir::AnalysisManager::new()).unwrap();
        assert!(c.get_op(load).is_live());
    }
    let source = SOURCE.replace("!i32","!i128").replace("    func.return %v", "    %zero = constant {value = 0} : !i128\n    %peer = addi %v, %zero : !i128\n    func.return %peer");
    let (c, f, _, load) = fixture(&source);
    pass.run(f, &c, &tir::AnalysisManager::new()).unwrap();
    assert!(c.has_operation(load));
    let ops = c
        .get_region(c.get_op(f).regions()[0])
        .iter(c.clone())
        .next()
        .unwrap()
        .op_ids();
    assert_eq!(
        c.get_op(*ops.last().unwrap()).value_operands()[0],
        c.get_op(load).value_results()[0]
    );
    let second = SOURCE[SOURCE.find("  %f").unwrap()..SOURCE.find("  module_end").unwrap()]
        .replace("@f", "@g");
    let source = SOURCE.replace("  module_end", &format!("{second}  module_end"));
    let (c, f, _, _) = fixture(&source);
    let module = c.parent_op(f).unwrap();
    let sibling = c.get_region(c.get_op(module).regions()[0]).op_ids()[1];
    let sibling_load = c
        .get_region(c.get_op(sibling).regions()[0])
        .iter(c.clone())
        .next()
        .unwrap()
        .op_ids()[2];
    pass.run(f, &c, &tir::AnalysisManager::new()).unwrap();
    assert!(c.get_op(sibling_load).is_live());
    let mixed = SOURCE.replace(
        "  module_end",
        &format!("{}  module_end", second.replace("func.return %v", "-> %v")),
    );
    let (mixed_context, mixed_function, _, covered_load) = fixture(&mixed);
    let mixed_module = mixed_context.parent_op(mixed_function).unwrap();
    let sibling = mixed_context
        .get_region(mixed_context.get_op(mixed_module).regions()[0])
        .op_ids()[1];
    let unsupported_load = mixed_context
        .get_region(mixed_context.get_op(sibling).regions()[0])
        .op_ids()[2];
    pass.run(mixed_module, &mixed_context, &tir::AnalysisManager::new())
        .unwrap();
    assert!(!mixed_context.has_operation(covered_load));
    assert!(mixed_context.has_operation(unsupported_load));
    let mixed_engine = mixed_context.egraph_with_rules().unwrap();
    use tir_egglog::Read;
    assert_eq!(mixed_engine.read(|s| s.table_size("OpKind")).unwrap(), 10);
    drop(mixed_engine);
    tir::verify_op_tree(&mixed_context, mixed_module).unwrap();
    let (c, f, _, _) = fixture(&source);
    let module = c.parent_op(f).unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let mut weighted = EqSatPass::new().with_operation_cost(move |name| {
        if name == "ptr.load" {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        1
    });
    weighted
        .run(module, &c, &tir::AnalysisManager::new())
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    let engine = c.egraph_with_rules().unwrap();
    assert_eq!(engine.read(|s| s.table_size("OpKind")).unwrap(), 10);
    drop(engine);
    tir::verify_op_tree(&c, module).unwrap();
}

#[test]
fn uncovered_memory_effects_preserve_writes_and_call_order() {
    use tir::Pass;
    let fixtures = [
        (
            "alias",
            include_str!("../../../../core/checks/Inputs/eqsat/dse_may_alias_parameter_kept.tir"),
        ),
        (
            "narrow",
            include_str!("../../../../core/checks/Inputs/eqsat/dse_narrower_overwrite_kept.tir"),
        ),
        (
            "call",
            include_str!("../../../../core/checks/Inputs/eqsat/dse_opaque_call_is_a_barrier.tir"),
        ),
        (
            "sub_byte",
            include_str!(
                "../../../../core/checks/Inputs/eqsat/dse_sub_byte_write_kills_nothing.tir"
            ),
        ),
    ];
    for (name, source) in fixtures {
        let c = Context::with_default_dialects();
        let module = tir::parse::ir::parse_operation(&c, source, &MODULE).unwrap();
        let f = c
            .get_region(module.regions()[0])
            .op_ids()
            .into_iter()
            .find(|&op| c.get_op(op).is_operation(&tir::func::FUNC))
            .unwrap();
        let body = c.get_region(c.get_op(f).regions()[0]);
        let effects = || {
            body.op_ids()
                .into_iter()
                .filter_map(|id| {
                    let op = c.get_op(id);
                    (op.is_operation(&tir::ptr::STORE) || op.is_operation(&tir::func::CALL))
                        .then(|| (op.name().to_string(), op.value_operands().to_vec()))
                })
                .collect::<Vec<_>>()
        };
        let original = effects();
        assert_eq!(
            original.iter().filter(|(kind, _)| kind == "store").count(),
            2
        );
        if name == "call" {
            assert_eq!(
                original
                    .iter()
                    .map(|(kind, _)| kind.as_str())
                    .collect::<Vec<_>>(),
                ["store", "call", "store"]
            );
        }
        for round in 0..2 {
            if round > 0 {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &tir::AnalysisManager::new())
                    .unwrap();
                assert_eq!(effects(), original, "{name}");
            }
            tir::verify_op_tree(&c, module.id()).unwrap();
            match name {
                "alias" => {
                    for alias in [false, true] {
                        let mut state = ExecutionState::default();
                        let p = state.memory.alloc(4, 4);
                        let q = if alias { p } else { state.memory.alloc(4, 4) };
                        let bits = 0x89abcdefu32;
                        let args = vec![
                            Value::Ptr(p),
                            Value::Ptr(q),
                            Value::Int(APInt::new(32, u64::from(bits))),
                        ];
                        assert!(interp::run_function_in_state(&c, f, args, &mut state, None)
                            .unwrap()
                            .is_empty());
                        assert_eq!(state.memory.read(p, 4).unwrap(), bits.to_le_bytes());
                        assert_eq!(state.memory.read(q, 4).unwrap(), bits.to_le_bytes());
                    }
                }
                "narrow" => {
                    let values = interp::run_function(
                        &c,
                        f,
                        vec![
                            Value::Int(APInt::new(64, 0x1122334455667788u64)),
                            Value::Int(APInt::new(32, 0xaabbccddu64)),
                        ],
                    )
                    .unwrap();
                    let [Value::Int(value)] = values.as_slice() else {
                        panic!("expected integer");
                    };
                    assert_eq!(
                        value.clone().with_signed(false),
                        APInt::new(64, 0x11223344aabbccddu64)
                    );
                }
                "sub_byte" => {
                    let values = interp::run_function(&c, f, vec![]).unwrap();
                    let [Value::Int(value)] = values.as_slice() else {
                        panic!("expected integer");
                    };
                    assert_eq!(value.clone().with_signed(false), APInt::new(8, 77));
                }
                "call" => {} // Its declaration supplies no interpreter body.
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn pointer_valued_forwarding_without_layout_preserves_the_load() {
    use tir::Pass;
    let (c, f, store, load) = fixture(include_str!(
        "../../../../core/checks/Inputs/eqsat/pointer_no_layout_keep.tir"
    ));
    tir::passes::EqSatPass::new()
        .run(f, &c, &tir::AnalysisManager::new())
        .unwrap();
    let ops = c.get_region(c.get_op(f).regions()[0]).op_ids();
    assert!(ops.contains(&store));
    assert!(ops.contains(&load));
    assert_eq!(
        c.get_op(*ops.last().unwrap()).value_operands()[0],
        c.get_op(load).value_results()[0]
    );
    assert_eq!(
        c.get_op(load).state_operands(),
        c.get_op(store).state_results()
    );
    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
}

#[test]
fn checked_pass_revalidates_real_source_and_rejects_another_candidate_proof() {
    use tir::passes::eqsat::{forward_memory, Capture, EqSatLibrary, EqSatPass};
    use tir::{AnalysisManager, Pass};
    const RULE: &str = "rule caller-forward: ptr.load(p: ptr<P> | ptr.store(x: int<32>, p | s: state<memory>)) => x proof smt;";
    for mutation in ["attribute", "chain", "movement", "request"] {
        let source = SOURCE.replace(
            "    %stored",
            "    %marker = constant {value = 7, keep = true} : !i32\n    %stored",
        );
        let source = if mutation == "request" {
            source.replace(
                "%marker = constant {value = 7, keep = true} : !i32",
                "%marker = state.entry_state : !state<memory>",
            )
        } else {
            source
        };
        let (c, f, marker, _) = fixture(&source);
        let mut pass =
            EqSatPass::with_library(EqSatLibrary::new(tir_pdl::compile(RULE).unwrap()).bind(
                "caller-forward",
                move |c, o, captures| {
                    let Capture::Operation(load) = captures[o.root] else {
                        return None;
                    };
                    let store = captures.iter().find_map(|capture| match capture {
                        Capture::Operation(op) if *op != load => Some(*op),
                        _ => None,
                    })?;
                    if mutation == "request" {
                        c.set_op_operand(store, 2, c.get_op(marker).state_results()[0]);
                    }
                    let proof = forward_memory(c, o, captures)?;
                    match mutation {
                        "request" => {}
                        "attribute" => {
                            let mut attrs = c.get_op(load).attributes();
                            attrs.push(c.named_attribute("volatile", true.into()));
                            c.set_op_attributes(load, attrs);
                        }
                        "chain" => c.set_op_operand(load, 1, c.get_op(store).state_operands()[0]),
                        "movement" => c.insert(c.insertion_point(marker), store),
                        _ => unreachable!(),
                    }
                    Some(proof)
                },
            ))
            .unwrap();
        let result = pass.run(f, &c, &AnalysisManager::new());
        eprintln!("real pass source control {mutation}: {result:?}");
        assert!(result.is_err());
        let output = printed_memory(&c, f);
        assert_eq!(
            output.matches("ptr.load").count(),
            1,
            "{mutation}: {output}"
        );
        assert_eq!(
            output.matches("ptr.store").count(),
            1,
            "{mutation}: {output}"
        );
    }
    let (c, f, _, load) = fixture(SOURCE);
    EqSatPass::new()
        .with_operation_cost(|_| 0)
        .run(f, &c, &AnalysisManager::new())
        .unwrap();
    assert!(c.has_operation(load));
    let second = SOURCE[SOURCE.find("  %f").unwrap()..SOURCE.find("  module_end").unwrap()]
        .replace("@f", "@g");
    let (c, f, _, _) = fixture(&SOURCE.replace("  module_end", &format!("{second}  module_end")));
    let module = c.parent_op(f).unwrap();
    let cached = std::sync::Mutex::new(None);
    let mut pass =
        EqSatPass::with_library(EqSatLibrary::new(tir_pdl::compile(RULE).unwrap()).bind(
            "caller-forward",
            move |c, o, captures| {
                let current = forward_memory(c, o, captures)?;
                let mut cached = cached.lock().unwrap();
                if cached.is_none() {
                    *cached = Some(current);
                    None
                } else {
                    cached.take()
                }
            },
        ))
        .unwrap();
    let result = pass.run(module, &c, &AnalysisManager::new());
    eprintln!("real pass other-candidate proof control: {result:?}");
    assert!(result.is_err());
    let engine = c.egraph_with_rules().unwrap();
    use tir_egglog::Read;
    assert_eq!(engine.read(|s| s.table_size("OpKind")).unwrap(), 12);
}

#[test]
fn checked_forwarding_preserves_effects_with_only_unused_nested_state_uses() {
    use tir::Pass;
    let source = SOURCE.replace(
        "    func.return %v",
        r#"    %dead = scf.switch %x {
      %observed, %last = ptr.load %p state(%loaded) : !i32
      -> %observed
    }
    {
      -> %x
    }
    -> %v"#,
    );
    let (before, f, _, _) = fixture(&source);
    let (after, g, store, load) = fixture(&source);
    tir::verify_op_tree(&before, before.parent_op(f).unwrap()).unwrap();
    let mut pass = tir::passes::EqSatPass::new();
    for _ in 0..2 {
        pass.run(g, &after, &tir::AnalysisManager::new()).unwrap();
        tir::verify_op_tree(&after, after.parent_op(g).unwrap()).unwrap();
        for bits in [0x12345678, 0, u32::MAX as u64] {
            for initialized in [true, false] {
                for poison in [false, true] {
                    for offset in [0, 1, 4, 5, 8] {
                        retained_store_case(
                            (&before, f),
                            (&after, g),
                            bits,
                            initialized,
                            poison,
                            offset,
                        );
                    }
                }
            }
        }
        assert!(after.get_op(load).is_live());
        assert!(after.get_op(store).is_live());
    }
}

fn composition_passes() -> [tir::passes::EqSatPass; 2] {
    use tir::passes::eqsat::{forward_memory, EqSatLibrary, EqSatPass};
    let rule = "rule caller-forward: ptr.load(p: ptr<P> | ptr.store(x: int<32>, p | s: state<memory>)) => x proof smt;";
    [
        EqSatPass::new(),
        EqSatPass::with_library(
            EqSatLibrary::new(tir_pdl::compile(rule).unwrap())
                .bind("caller-forward", forward_memory),
        )
        .unwrap(),
    ]
}

fn printed_memory(c: &Context, f: OpId) -> String {
    let module = c
        .get_op(c.parent_op(f).unwrap())
        .as_operation(&MODULE)
        .unwrap();
    let mut text = String::new();
    tir::print_ir(&module, c, &mut tir::IRFormatter::new(&mut text)).unwrap();
    text
}

fn composed_execution(
    before: (&Context, OpId),
    after: (&Context, OpId),
    expected: impl Fn(u32) -> (u32, u32),
    ignores_x: bool,
) {
    for (bits, initialized, poison, offset) in [
        (0, false, false, 0),
        (1, true, false, 4),
        (0x12345678, false, false, 1),
        (u32::MAX, true, false, 0),
        (1, false, true, 0),
        (1, true, true, 1),
        (1, true, false, 5),
        (1, false, false, 8),
    ] {
        let mut a = ExecutionState::default();
        let mut b = ExecutionState::default();
        let base = a.memory.alloc(8, 1);
        assert_eq!(base, b.memory.alloc(8, 1));
        if initialized {
            a.memory.write(base, &[0xa5; 8]).unwrap();
            b.memory.write(base, &[0xa5; 8]).unwrap();
        }
        let x = if poison {
            Value::Poison
        } else {
            Value::Int(APInt::new(32, bits.into()))
        };
        let function = before
            .0
            .get_op(before.1)
            .as_operation(&tir::func::FUNC)
            .unwrap();
        let args: Vec<_> = tir::func::func_parameters(&function)
            .iter()
            .map(|parameter| {
                if tir::ptr::PtrType::view(before.0, parameter.ty()).is_some() {
                    Value::Ptr(base + offset)
                } else {
                    x.clone()
                }
            })
            .collect();
        let av = interp::run_function_in_state(before.0, before.1, args.clone(), &mut a, None);
        let bv = interp::run_function_in_state(after.0, after.1, args, &mut b, None);
        eprintln!(
            "composition bits={bits} init={initialized} poison={poison} offset={offset} source={av:?} published={bv:?}"
        );
        match (av.as_deref(), bv.as_deref()) {
            (Ok([Value::Int(a)]), Ok([Value::Int(b)])) => {
                assert_eq!(a.width(), b.width());
                assert_eq!(a.to_u64(), b.to_u64());
            }
            (Ok([Value::Poison]), Ok([Value::Poison])) => {}
            _ => assert_eq!(format!("{av:?}"), format!("{bv:?}")),
        }
        let (result, word) = expected(bits);
        if offset <= 4 && (!poison || ignores_x) {
            let values = bv.as_ref().unwrap();
            let [Value::Int(actual)] = values.as_slice() else {
                panic!("expected integer {values:?}")
            };
            assert_eq!(actual.to_u64(), u64::from(result));
            assert_eq!(b.memory.read(base + offset, 4).unwrap(), word.to_le_bytes());
        }
        for i in 0..8 {
            let cell = b.memory.read(base + i, 1);
            assert_eq!(
                format!("{:?}", a.memory.read(base + i, 1)),
                format!("{cell:?}")
            );
            let modified = offset <= 4 && (offset..offset + 4).contains(&i);
            let byte = if modified && (!poison || ignores_x) {
                Some(((word >> ((i - offset) * 8)) & 255) as u8)
            } else if !modified && initialized {
                Some(0xa5)
            } else {
                None
            };
            match byte {
                Some(byte) => assert_eq!(cell.unwrap(), [byte]),
                None => assert!(matches!(
                    cell,
                    Err(interp::InterpError::Uninitialized { .. })
                )),
            }
            eprintln!("cell {i}: {:?}", b.memory.read(base + i, 1));
        }
        if offset > 4 {
            assert!(bv.is_err());
        } else if poison && !ignores_x {
            assert!(matches!(bv.as_ref().unwrap().as_slice(), [Value::Poison]));
        }
    }
}

#[test]
fn checked_forwarding_composes_shared_split_and_joined_rmw_effects() {
    let shared =
        include_str!("../../../../core/checks/Inputs/eqsat/composition-shared-unordered.tir");
    let triple = shared
        .replace(" %j =", " %c, %u = ptr.load %p state(%s) : !i32\n %j =")
        .replace("state(%r, %t)", "state(%u, %r, %t)")
        .replace(" -> %sum, %j", " %all = addi %sum, %c : !i32\n -> %all, %j");
    let split = include_str!("../../../../core/checks/Inputs/eqsat/composition-split-direct.tir");
    let excluded =
        include_str!("../../../../core/checks/Inputs/eqsat/composition-split-intervening.tir");
    let rmw = include_str!("../../../../core/checks/Inputs/eqsat/composition-rmw-unordered.tir");
    let volatile = rmw.replace("state(%next) :", "state(%next) {volatile = true} :");
    for mut pass in composition_passes() {
        for (kind, source, factor) in [
            ("shared", shared, 2u32),
            ("shared", triple.as_str(), 3),
            ("split", split, 3),
            ("excluded", excluded, 3),
            ("rmw", rmw, 2),
            ("volatile", volatile.as_str(), 2),
        ] {
            for ordered in [false, true] {
                composed_flat_case(&mut pass, kind, source, factor, ordered);
            }
        }
    }
}

fn composed_flat_case(
    pass: &mut tir::passes::EqSatPass,
    kind: &str,
    source: &str,
    factor: u32,
    ordered: bool,
) {
    use tir::{AnalysisManager, Pass};
    let writes_sum = ["rmw", "volatile"].contains(&kind);
    let source = if ordered {
        source
            .replace("-> %sum, %j", "func.return %sum state(%j)")
            .replace("-> %all, %j", "func.return %all state(%j)")
            .replace("-> %out, %final", "func.return %out state(%final)")
    } else {
        source.to_owned()
    };
    let (before, f, _, _) = fixture(&source);
    let (c, g, store, _) = fixture(&source);
    let module = c.parent_op(g).unwrap();
    tir::verify_op_tree(&before, before.parent_op(f).unwrap()).unwrap();
    tir::verify_op_tree(&c, module).unwrap();
    let region = c.get_op(g).regions()[0];
    let ops = c.get_region(region).op_ids();
    let loads: Vec<_> = ops
        .iter()
        .copied()
        .filter(|&op| c.get_op(op).name().as_str() == "load")
        .collect();
    let join = *ops
        .iter()
        .find(|&&op| c.get_op(op).name().as_str() == "join")
        .unwrap();
    let sums: Vec<_> = ops
        .iter()
        .copied()
        .filter(|&op| c.get_op(op).name().as_str() == "addi")
        .collect();
    let x = c.get_op(store).value_operands()[0];
    let state = c.get_op(store).state_results()[0];
    let removed = match kind {
        "shared" | "rmw" => loads.clone(),
        "volatile" => loads[..2].to_vec(),
        "split" => loads[..1].to_vec(),
        _ => vec![],
    };
    let retained: Vec<_> = ops
        .iter()
        .copied()
        .filter(|op| !removed.contains(op))
        .collect();
    let results: Vec<_> = retained.iter().map(|&op| c.get_op(op).results()).collect();
    let unchanged: Vec<_> = retained
        .iter()
        .copied()
        .filter(|&op| {
            op != join
                && !sums.contains(&op)
                && c.get_op(op).name().as_str() != "split"
                && !(writes_sum && c.get_op(op).name().as_str() == "return")
        })
        .map(|op| (op, c.get_op(op).operands()))
        .collect();
    let yields = c.get_region(region).results();
    let original = printed_memory(&c, g);
    pass.run(g, &c, &AnalysisManager::new()).unwrap();
    tir::verify_op_tree(&c, module).unwrap();
    let surviving: Vec<_> = removed
        .iter()
        .copied()
        .filter(|&load| c.has_operation(load))
        .collect();
    assert_eq!(
        surviving,
        Vec::<OpId>::new(),
        "covered load retained: {kind} ordered={ordered}"
    );
    assert_eq!(c.get_region(region).op_ids(), retained);
    for (&op, expected) in retained.iter().zip(results) {
        assert_eq!(c.get_op(op).results(), expected);
    }
    for (op, operands) in unchanged {
        assert_eq!(c.get_op(op).operands(), operands);
    }
    if !writes_sum {
        assert_eq!(c.get_region(region).results(), yields);
    }
    match kind {
        "shared" => {
            assert_eq!(
                c.get_op(join).state_operands().as_slice(),
                vec![state; factor as usize]
            );
            assert_eq!(c.get_op(sums[0]).value_operands().as_slice(), [x, x]);
            if factor == 3 {
                assert_eq!(
                    c.get_op(sums[1]).value_operands().as_slice(),
                    [c.get_op(sums[0]).value_results()[0], x]
                );
            }
        }
        "split" => {
            let split = ops[3];
            assert_eq!(c.get_op(split).state_operands().as_slice(), [state]);
            let branches = c.get_op(split).state_results();
            assert_eq!(branches.len(), 2);
            assert_eq!(
                c.get_op(loads[1]).state_operands().as_slice(),
                [branches[0]]
            );
            assert_eq!(
                c.get_op(loads[2]).state_operands().as_slice(),
                [branches[1]]
            );
            assert_eq!(
                c.get_op(join).state_operands().as_slice(),
                [
                    c.get_op(loads[1]).state_results()[0],
                    c.get_op(loads[2]).state_results()[0]
                ]
            );
            assert_eq!(
                c.get_op(sums[0]).value_operands().as_slice(),
                [x, c.get_op(loads[1]).value_results()[0]]
            );
            assert_eq!(
                c.get_op(sums[1]).value_operands().as_slice(),
                [
                    c.get_op(sums[0]).value_results()[0],
                    c.get_op(loads[2]).value_results()[0]
                ]
            );
        }
        "rmw" | "volatile" => assert_rmw_publication(&c, &ops, ordered, kind == "volatile"),
        _ => assert_eq!(printed_memory(&c, g), original),
    }
    composed_execution(
        (&before, f),
        (&c, g),
        |x| {
            (
                x.wrapping_mul(factor),
                if writes_sum { x.wrapping_mul(2) } else { x },
            )
        },
        false,
    );
    let published = printed_memory(&c, g);
    eprintln!("flat {kind} ordered={ordered}:\n{published}");
    pass.run(g, &c, &AnalysisManager::new()).unwrap();
    tir::verify_op_tree(&c, module).unwrap();
    assert_eq!(printed_memory(&c, g), published);
}

#[test]
fn checked_forwarding_composes_carried_loop_yields() {
    let source = include_str!("../../../../core/checks/Inputs/eqsat/composition-loop-carried.tir");
    for mut pass in composition_passes() {
        for excluded in [false, true] {
            composed_loop_case(&mut pass, source, excluded);
        }
    }
}

fn composed_loop_case(pass: &mut tir::passes::EqSatPass, source: &str, excluded: bool) {
    use tir::{AnalysisManager, Pass};
    let source = if excluded {
        source.replace(
            "      %13, %done = ptr.load %3 state(%12)",
            "      %split = state.split state(%12)\n      %13, %done = ptr.load %3 state(%split)",
        )
    } else {
        source.to_owned()
    };
    let (before, f, _, _) = fixture(&source);
    let (c, g, _, _) = fixture(&source);
    let module = c.parent_op(g).unwrap();
    tir::verify_op_tree(&before, before.parent_op(f).unwrap()).unwrap();
    tir::verify_op_tree(&c, module).unwrap();
    let outer = c.get_op(g).regions()[0];
    let ops = c.get_region(outer).op_ids();
    let loop_op = ops[5];
    let body = c.get_op(loop_op).regions()[0];
    let inner = c.get_region(body).op_ids();
    let load = inner[if excluded { 7 } else { 6 }];
    assert_eq!(c.get_op(load).name().as_str(), "load");
    let update = inner[4];
    let store = inner[5];
    let condition = *inner.last().unwrap();
    let original_yields = c.get_region(body).results();
    let carried = c.get_op(inner[0]).state_operands();
    assert_eq!(
        c.get_op(inner[1]).state_operands().as_slice(),
        carried.as_slice()
    );
    let join_inputs = [
        c.get_op(inner[0]).state_results()[0],
        c.get_op(inner[1]).state_results()[0],
    ];
    let unchanged: Vec<_> = ops
        .iter()
        .chain(&inner)
        .copied()
        .filter(|&op| op != load && op != condition)
        .map(|op| (op, c.get_op(op).operands(), c.get_op(op).results()))
        .collect();
    let condition_rhs = c.get_op(condition).value_operands()[1];
    let condition_results = c.get_op(condition).results();
    let original = printed_memory(&c, g);
    pass.run(g, &c, &AnalysisManager::new()).unwrap();
    tir::verify_op_tree(&c, module).unwrap();
    assert_eq!(c.get_region(outer).op_ids(), ops);
    for (op, operands, results) in unchanged {
        assert_eq!(c.get_op(op).operands(), operands);
        assert_eq!(c.get_op(op).results(), results);
    }
    assert_eq!(c.get_op(condition).value_operands()[1], condition_rhs);
    assert_eq!(c.get_op(condition).results(), condition_results);
    if excluded {
        assert_eq!(printed_memory(&c, g), original);
    } else {
        assert!(
            !c.has_operation(load),
            "covered load retained: carried loop"
        );
        assert_eq!(
            c.get_region(body).op_ids(),
            inner
                .iter()
                .copied()
                .filter(|&op| op != load)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            c.get_op(condition).value_operands()[0],
            c.get_op(update).value_results()[0]
        );
        assert_eq!(
            c.get_region(body).results(),
            [
                original_yields[0],
                original_yields[1],
                c.get_op(store).state_results()[0],
                c.get_op(update).value_results()[0],
                c.get_op(store).state_results()[0]
            ]
        );
    }
    assert_eq!(
        c.get_op(inner[0]).state_operands().as_slice(),
        carried.as_slice()
    );
    assert_eq!(
        c.get_op(inner[1]).state_operands().as_slice(),
        carried.as_slice()
    );
    assert_eq!(c.get_op(inner[2]).state_operands().as_slice(), join_inputs);
    assert_eq!(
        c.get_op(store).state_operands().as_slice(),
        c.get_op(inner[2]).state_results().as_slice()
    );
    assert_eq!(
        c.get_op(store).value_operands()[0],
        c.get_op(update).value_results()[0]
    );
    assert_eq!(
        c.get_op(ops[6]).state_operands().as_slice(),
        c.get_op(loop_op).state_results().as_slice()
    );
    composed_execution((&before, f), (&c, g), |_| (6, 3), true);
    let published = printed_memory(&c, g);
    eprintln!("loop excluded={excluded}:\n{published}");
    pass.run(g, &c, &AnalysisManager::new()).unwrap();
    tir::verify_op_tree(&c, module).unwrap();
    assert_eq!(printed_memory(&c, g), published);
}

fn assert_rmw_publication(c: &Context, ops: &[OpId], ordered: bool, volatile: bool) {
    let store = ops[1];
    let region = c.region_of_op(store).unwrap();
    let join = ops[4];
    let sum = ops[5];
    let later_store = ops[6];
    let final_load = ops[7];
    let x = c.get_op(store).value_operands()[0];
    let state = c.get_op(store).state_results()[0];
    assert_eq!(c.get_op(join).state_operands().as_slice(), [state, state]);
    assert_eq!(c.get_op(sum).value_operands().as_slice(), [x, x]);
    assert_eq!(
        c.get_op(later_store).state_operands().as_slice(),
        c.get_op(join).state_results().as_slice()
    );
    assert_eq!(
        c.get_op(later_store).value_operands().as_slice(),
        [
            c.get_op(sum).value_results()[0],
            c.get_op(store).value_operands()[1]
        ]
    );
    let (value, state) = if volatile {
        assert_eq!(
            c.get_op(final_load).state_operands().as_slice(),
            c.get_op(later_store).state_results().as_slice()
        );
        (
            c.get_op(final_load).value_results()[0],
            c.get_op(final_load).state_results()[0],
        )
    } else {
        (
            c.get_op(sum).value_results()[0],
            c.get_op(later_store).state_results()[0],
        )
    };
    if ordered {
        assert_eq!(
            c.get_op(*ops.last().unwrap()).operands().as_slice(),
            [value, state]
        );
    } else {
        assert_eq!(c.get_region(region).results(), [value, state]);
    }
}

#[test]
fn moved_memory_guards_preserve_read_types_addresses_and_cells() {
    use tir::utils::APFloat;
    use tir::{AnalysisManager, Pass};

    let width = include_str!("../../../../core/checks/EqSat/store_forward_width.tir");
    let types = include_str!("../../../../core/checks/EqSat/store_forward_type.tir");
    let address = include_str!("../../../../core/checks/EqSat/store_forward_address.tir");
    let layout = "module {layout.spec = #layout.spec<endianness = \"little\", !ptr.p = {size = 64, abi = 64}>} {";
    let width_domain = width
        .replace("module {", layout)
        .replace("!ptr.p,", "!ptr.p<!i32>,")
        .replace("%4 = ptr.load", "%4, %read = ptr.load");
    let type_domain = types
        .replace("module {", layout)
        .replace("!ptr.p,", "!ptr.p<!f32>,")
        .replace("%4 = ptr.load", "%4, %read = ptr.load");
    let address_domain = address
        .replace("module {", layout)
        .replace("%5 = ptr.load", "%5, %read = ptr.load");
    let float = Value::Float(APFloat::from_bits(8, 23, false, 0x3f80_0000));
    let int = |width, bits| Value::Int(APInt::new(width, bits));
    let cases = [
        (
            "original-width",
            width.to_owned(),
            false,
            false,
            int(16, 0x5678),
        ),
        (
            "width-only",
            width_domain.clone(),
            false,
            false,
            int(16, 0x5678),
        ),
        (
            "width-positive",
            width_domain.replace("!i16", "!i32"),
            true,
            false,
            int(32, 0x1234_5678),
        ),
        (
            "original-type",
            types.to_owned(),
            false,
            true,
            int(32, 0x3f80_0000),
        ),
        (
            "unsupported-float-store",
            type_domain.clone(),
            false,
            true,
            int(32, 0x3f80_0000),
        ),
        (
            "type-only",
            width_domain.replace("!i16", "!f32"),
            false,
            false,
            Value::Float(APFloat::from_bits(8, 23, false, 0x3f80_0000)),
        ),
        (
            "type-positive",
            type_domain.replace("!f32", "!i32"),
            true,
            false,
            int(32, 0x1234_5678),
        ),
        (
            "original-address",
            address.to_owned(),
            false,
            false,
            int(32, 0x1234_5678),
        ),
        (
            "address-only",
            address_domain.clone(),
            false,
            false,
            int(32, 0x1234_5678),
        ),
        (
            "address-positive",
            address_domain.replace("ptr.load %1", "ptr.load %0"),
            true,
            false,
            int(32, 0x1234_5678),
        ),
    ];
    for (pass_index, mut pass) in composition_passes().into_iter().enumerate() {
        for (name, source, forwards, float_store, expected) in &cases {
            let (before, f, _, _) = fixture(source);
            let (c, g, store, load) = fixture(source);
            let module = c.parent_op(g).unwrap();
            tir::verify_op_tree(&before, before.parent_op(f).unwrap()).unwrap();
            tir::verify_op_tree(&c, module).unwrap();
            let region = c.get_op(g).regions()[0];
            let ops = c.get_region(region).op_ids();
            let ret = *ops.last().unwrap();
            let original = printed_memory(&c, g);
            let identities: Vec<_> = ops
                .iter()
                .map(|&op| (op, c.get_op(op).operands(), c.get_op(op).results()))
                .collect();
            let input = c.get_op(store).value_operands()[0];
            let stored_state = c.get_op(store).state_results();
            assert_eq!(c.get_op(load).state_operands(), stored_state);
            assert_eq!(
                c.get_op(ret).value_operands(),
                c.get_op(load).value_results()
            );
            pass.run(g, &c, &AnalysisManager::new()).unwrap();
            // Check publication before accepting the output verifier as evidence.
            if *forwards {
                assert!(!c.has_operation(load), "{name} pass={pass_index}");
                assert_eq!(c.get_op(ret).value_operands().as_slice(), [input]);
                assert_eq!(
                    c.get_region(region).op_ids(),
                    ops.iter()
                        .copied()
                        .filter(|&op| op != load)
                        .collect::<Vec<_>>()
                );
                assert_eq!(c.get_op(store).state_results(), stored_state);
            } else {
                assert!(c.has_operation(load), "{name} pass={pass_index}");
                for (op, operands, results) in &identities {
                    assert_eq!(c.get_op(*op).operands(), *operands);
                    assert_eq!(c.get_op(*op).results(), *results);
                }
                assert_eq!(c.get_region(region).op_ids(), ops);
                assert_eq!(printed_memory(&c, g), original);
            }
            tir::verify_op_tree(&c, module).unwrap();
            eprintln!(
                "guard {name} pass={pass_index} forwards={forwards}\n{}",
                printed_memory(&c, g)
            );
            let distinct_ssa = name.contains("address");
            let bits = if *float_store || *name == "type-only" {
                0x3f80_0000
            } else {
                0x1234_5678
            };
            for &separate in if distinct_ssa {
                &[false, true][..]
            } else {
                &[false][..]
            } {
                let expected = if distinct_ssa && separate && !forwards {
                    int(32, 0xdead_beef)
                } else {
                    expected.clone()
                };
                let value = if *float_store {
                    float.clone()
                } else {
                    int(32, bits)
                };
                let bytes = if bits == 0x3f80_0000 {
                    [0xa5, 0xa5, 0, 0, 0x80, 0x3f, 0xa5, 0xa5]
                } else {
                    [0xa5, 0xa5, 0x78, 0x56, 0x34, 0x12, 0xa5, 0xa5]
                };
                for (context, function) in [(&before, f), (&c, g)] {
                    guard_read_execution(
                        (context, function),
                        name,
                        distinct_ssa,
                        separate,
                        &value,
                        &expected,
                        bytes,
                    );
                }
            }
        }
    }
}

fn guard_read_execution(
    (context, function): (&Context, OpId),
    name: &str,
    distinct_ssa: bool,
    separate: bool,
    value: &Value,
    expected: &Value,
    bytes: [u8; 8],
) {
    let mut state = ExecutionState::default();
    let base = state.memory.alloc(8, 1);
    state.memory.write(base, &[0xa5; 8]).unwrap();
    let other = state.memory.alloc(8, 1);
    state
        .memory
        .write(other, &[0x5a, 0x5a, 0xef, 0xbe, 0xad, 0xde, 0x5a, 0x5a])
        .unwrap();
    let write = base + 2;
    let read = if separate { other + 2 } else { write };
    let args = if distinct_ssa {
        vec![Value::Ptr(write), Value::Ptr(read), value.clone()]
    } else {
        vec![Value::Ptr(write), value.clone()]
    };
    let actual = interp::run_function_in_state(context, function, args, &mut state, None).unwrap();
    match (actual.as_slice(), expected) {
        ([Value::Int(actual)], Value::Int(expected)) => {
            assert_eq!(actual, expected, "read oracle {name}")
        }
        ([Value::Float(actual)], Value::Float(expected)) => {
            assert_eq!(actual, expected, "read oracle {name}")
        }
        _ => panic!("read type oracle {name}: {actual:?}, expected {expected:?}"),
    }
    assert_eq!(state.memory.read(base, 8).unwrap(), bytes);
    assert_eq!(
        state.memory.read(other, 8).unwrap(),
        [0x5a, 0x5a, 0xef, 0xbe, 0xad, 0xde, 0x5a, 0x5a]
    );
    eprintln!("read {name} separate={separate} actual={actual:?} cells={bytes:?}");
}

#[test]
fn moved_reads_keep_their_memory_version_across_a_store() {
    use tir::{AnalysisManager, Pass};
    let cases = [
        (
            "different-versions",
            include_str!("../../../../core/checks/Inputs/eqsat/load_no_cse_across_store.tir"),
            true,
            0x8888_8889,
        ),
        (
            "first-read-returned",
            include_str!("../../../../core/checks/Inputs/eqsat/load_midchain_kept.tir"),
            false,
            0x1234_5678,
        ),
    ];
    for (name, original, difference, expected) in cases {
        for layout in [false, true] {
            let source = if layout {
                original
                    .replace("module {", "module {layout.spec = #layout.spec<endianness = \"little\", !ptr.p = {size = 64, abi = 64}>} {")
                    .replace("%6 = ptr.load", "%6, %post = ptr.load")
            } else {
                original.to_owned()
            };
            let (before, f, _, _) = fixture(&source);
            let (c, g, first, store) = fixture(&source);
            let module = c.parent_op(g).unwrap();
            tir::verify_op_tree(&before, before.parent_op(f).unwrap()).unwrap();
            tir::verify_op_tree(&c, module).unwrap();
            let ops = c.get_region(c.get_op(g).regions()[0]).op_ids();
            let old_value = c.get_op(first).value_results()[0];
            let old_state = c.get_op(first).state_results();
            let new_value = c.get_op(store).value_operands()[0];
            let new_state = c.get_op(store).state_results();
            assert_eq!(c.get_op(store).state_operands(), old_state);
            assert_ne!(old_state, new_state);
            tir::passes::EqSatPass::new()
                .run(g, &c, &AnalysisManager::new())
                .unwrap();
            assert!(c.has_operation(first), "{name} layout={layout}");
            assert!(c.has_operation(store), "{name} layout={layout}");
            assert_eq!(c.get_op(store).state_operands(), old_state);
            let ret = *ops.last().unwrap();
            if difference {
                let post = ops[3];
                let sub = ops[4];
                assert!(!c.has_operation(post), "{name} layout={layout}");
                let rhs = new_value;
                assert_eq!(c.get_op(sub).value_operands().as_slice(), [old_value, rhs]);
            } else {
                assert_eq!(c.get_op(ret).value_operands().as_slice(), [old_value]);
                assert_eq!(c.get_op(ret).state_operands(), new_state);
            }
            tir::verify_op_tree(&c, module).unwrap();
            eprintln!("versions {name} layout={layout}\n{}", printed_memory(&c, g));
            for (context, function) in [(&before, f), (&c, g)] {
                for initialized in [false, true] {
                    let mut state = ExecutionState::default();
                    let base = state.memory.alloc(8, 1);
                    state.memory.write(base, &[0xa5; 2]).unwrap();
                    state.memory.write(base + 6, &[0xa5; 2]).unwrap();
                    if initialized {
                        state
                            .memory
                            .write(base + 2, &[0x78, 0x56, 0x34, 0x12])
                            .unwrap();
                    } else {
                        assert!(matches!(
                            state.memory.read(base + 2, 4),
                            Err(interp::InterpError::Uninitialized { .. })
                        ));
                    }
                    let values = interp::run_function_in_state(
                        context,
                        function,
                        vec![
                            Value::Ptr(base + 2),
                            Value::Int(APInt::new(32, 0x89ab_cdef)),
                        ],
                        &mut state,
                        None,
                    )
                    .unwrap();
                    match (values.as_slice(), initialized) {
                        ([Value::Int(value)], true) => assert_eq!(value.to_u64(), expected),
                        ([Value::Poison], false) => {}
                        _ => panic!("{name} layout={layout} initialized={initialized}: {values:?}"),
                    }
                    assert_eq!(
                        state.memory.read(base, 8).unwrap(),
                        [0xa5, 0xa5, 0xef, 0xcd, 0xab, 0x89, 0xa5, 0xa5]
                    );
                    eprintln!(
                        "versions {name} layout={layout} initialized={initialized}: {values:?}, bytes={:?}",
                        state.memory.read(base, 8).unwrap()
                    );
                }
                let mut state = ExecutionState::default();
                assert!(interp::run_function_in_state(
                    context,
                    function,
                    vec![Value::Ptr(0), Value::Int(APInt::new(32, 0x89ab_cdef))],
                    &mut state,
                    None
                )
                .is_err());
            }
        }
    }
}

#[test]
fn ordinary_memory_family_forwards_values_and_preserves_cells() {
    use tir::Pass;
    let pointer = include_str!("../../../../core/checks/Inputs/eqsat/store_forward_pointer.tir");
    let mut cases = vec![(pointer.to_owned(), 64, true)];
    for (width, explicit_endian) in [8, 16, 32, 64]
        .into_iter()
        .flat_map(|width| [(width, false), (width, true)])
    {
        let mut source = SOURCE.replace("!i32", &format!("!i{width}"));
        if !explicit_endian {
            source = source.replace("endianness = \"little\", ", "");
        }
        cases.push((source, width, false));
    }
    for (source, width, is_pointer) in cases {
        let (before, f, _, _) = fixture(&source);
        let (after, g, store, load) = fixture(&source);
        tir::passes::EqSatPass::new()
            .run(g, &after, &tir::AnalysisManager::new())
            .unwrap();
        assert!(
            !after.has_operation(load),
            "width={width} pointer={is_pointer}\n{source}"
        );
        assert!(after.get_op(store).is_live());
        tir::verify_op_tree(&after, after.parent_op(g).unwrap()).unwrap();
        for bits in [0, u64::MAX, 0x9876543212345678] {
            let bits = APInt::new(width, bits).to_u64();
            for (initialized, poison) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                for offset in [0, 1, 16 - width / 8, 17 - width / 8, u64::MAX] {
                    let mut a = ExecutionState::default();
                    let mut b = ExecutionState::default();
                    let base = a.memory.alloc(16, 1);
                    assert_eq!(base, b.memory.alloc(16, 1));
                    if initialized {
                        a.memory.write(base, &[0xa5; 16]).unwrap();
                        b.memory.write(base, &[0xa5; 16]).unwrap();
                    }
                    let address = base.wrapping_add(offset);
                    let value = match (poison, is_pointer) {
                        (true, _) => Value::Poison,
                        (false, true) => Value::Ptr(bits),
                        (false, false) => Value::Int(APInt::new(width, bits)),
                    };
                    let args = if is_pointer {
                        vec![Value::Ptr(address), value.clone()]
                    } else {
                        vec![value.clone(), Value::Ptr(address)]
                    };
                    let av = interp::run_function_in_state(&before, f, args.clone(), &mut a, None);
                    let bv = interp::run_function_in_state(&after, g, args, &mut b, None);
                    assert_eq!(format!("{av:?}"), format!("{bv:?}"));
                    let valid = offset <= 16 - width / 8;
                    if valid {
                        assert_eq!(format!("{:?}", av.unwrap()), format!("{:?}", vec![value]));
                    } else {
                        assert!(av.is_err());
                    }
                    for i in 0..16 {
                        let acell = a.memory.read(base + i, 1);
                        let bcell = b.memory.read(base + i, 1);
                        assert_eq!(format!("{acell:?}"), format!("{bcell:?}"));
                        match (
                            valid && (offset..offset + width / 8).contains(&i),
                            poison,
                            initialized,
                        ) {
                            (true, false, _) => assert_eq!(
                                bcell.unwrap(),
                                [bits.to_le_bytes()[(i - offset) as usize]]
                            ),
                            (false, _, true) => assert_eq!(bcell.unwrap(), [0xa5]),
                            (true, true, _) | (false, _, false) => assert!(bcell.is_err()),
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn ordinary_memory_family_rejects_unsupported_layouts_and_types() {
    use tir::Pass;
    let pointer = include_str!("../../../../core/checks/Inputs/eqsat/store_forward_pointer.tir");
    // The pointer law needs an explicit address width, and no type can opt into BE.
    for source in [
        pointer.replace("module {layout.spec = #layout_spec}", "module"),
        pointer.replace("size = 64, abi = 64", "size = 32, abi = 32"),
        pointer.replace("#layout.spec<", "#layout.spec<endianness = \"big\", "),
        SOURCE.replace("!i32", "!i1"),
    ] {
        let (c, f, s, l) = fixture(&source);
        assert!(check_forward(&c, f, s, l).is_err());
        tir::passes::EqSatPass::new()
            .run(f, &c, &tir::AnalysisManager::new())
            .unwrap();
        assert!(c.has_operation(l));
    }
}

#[test]
fn shared_loads_cannot_supply_each_others_removed_store_demand() {
    use tir::Pass;
    let source = SOURCE.replace("%v, %loaded = ptr.load %p state(%stored) : !i32", "%first, %first_state = ptr.load %p state(%stored) : !i32\n    %v, %loaded = ptr.load %p state(%stored) : !i32\n    %sum = addi %first, %v : !i32")
        .replace("func.return %v", "-> %sum");
    for optimize in [false, true] {
        let (c, f, _, _) = fixture(&source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        if optimize {
            tir::passes::EqSatPass::new()
                .run(f, &c, &tir::AnalysisManager::new())
                .unwrap();
        }
        let mut state = ExecutionState::default();
        let address = state.memory.alloc(4, 4);
        state.memory.write(address, &[0xa5; 4]).unwrap();
        let result = interp::run_function_in_state(
            &c,
            f,
            vec![Value::Int(APInt::new(32, 7)), Value::Ptr(address)],
            &mut state,
            None,
        )
        .unwrap();
        let [Value::Int(value)] = result.as_slice() else {
            panic!("expected integer");
        };
        assert_eq!(value.with_signed(false), APInt::new(32, 14));
        assert_eq!(state.memory.read(address, 4).unwrap(), [7, 0, 0, 0]);
    }
}

#[test]
fn store_overwrites_preserve_readable_cells_and_poison() {
    use tir::{AnalysisManager, Pass};
    for (source, pointer) in [
        (
            include_str!("../../../../core/checks/Inputs/eqsat/dead_store.tir"),
            false,
        ),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/dead_store_pointer.tir"),
            true,
        ),
    ] {
        for source in [
            source.to_owned(),
            source.replace(
                "%5 = ptr.store %2, %0 state(%4)",
                "%middle = ptr.store %1, %0 state(%4)\n    %5 = ptr.store %2, %0 state(%middle)",
            ),
        ] {
            let c = Context::with_default_dialects();
            let module = tir::parse::ir::parse_operation(&c, &source, &MODULE).unwrap();
            let f = c.get_region(module.regions()[0]).op_ids()[0];
            for round in 0..2 {
                if round > 0 {
                    tir::passes::EqSatPass::new()
                        .run(f, &c, &AnalysisManager::new())
                        .unwrap();
                    assert_eq!(
                        c.get_region(c.get_op(f).regions()[0])
                            .op_ids()
                            .iter()
                            .filter(|&&id| c.get_op(id).is_operation(&tir::ptr::STORE))
                            .count(),
                        1
                    );
                }
                tir::verify_op_tree(&c, module.id()).unwrap();
                for first_poison in [false, true] {
                    for last_poison in [false, true] {
                        let mut state = ExecutionState::default();
                        let width = if pointer { 8 } else { 4 };
                        let p = state.memory.alloc(width, width);
                        state.memory.write(p, &vec![0xa5; width as usize]).unwrap();
                        let first = if first_poison {
                            Value::Poison
                        } else if pointer {
                            Value::Ptr(p + 1)
                        } else {
                            Value::Int(APInt::new(32, 0x12345678))
                        };
                        let last = if last_poison {
                            Value::Poison
                        } else if pointer {
                            Value::Ptr(p + 2)
                        } else {
                            Value::Int(APInt::new(32, 0x89abcdefu64))
                        };
                        assert!(interp::run_function_in_state(
                            &c,
                            f,
                            vec![Value::Ptr(p), first, last],
                            &mut state,
                            None
                        )
                        .unwrap()
                        .is_empty());
                        let cell = state.memory.read(p, width);
                        if last_poison {
                            assert!(matches!(
                                cell,
                                Err(interp::InterpError::Uninitialized { .. })
                            ));
                        } else {
                            let expected = if pointer {
                                (p + 2).to_le_bytes().to_vec()
                            } else {
                                0x89abcdefu32.to_le_bytes().to_vec()
                            };
                            assert_eq!(cell.unwrap(), expected);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn overwritten_store_before_a_trap_remains_observable() {
    use tir::{AnalysisManager, Pass};
    let source = include_str!("../../../../core/checks/Inputs/eqsat/dead_store.tir").replace(
        "%5 = ptr.store",
        "%trap = divsi %1, %2 : !i32\n    %5 = ptr.store",
    );
    let c = Context::with_default_dialects();
    let module = tir::parse::ir::parse_operation(&c, &source, &MODULE).unwrap();
    let f = c.get_region(module.regions()[0]).op_ids()[0];
    for round in 0..2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
        }
        let mut state = ExecutionState::default();
        let p = state.memory.alloc(4, 4);
        let result = interp::run_function_in_state(
            &c,
            f,
            vec![
                Value::Ptr(p),
                Value::Int(APInt::new(32, 15)),
                Value::Int(APInt::new(32, 0)),
            ],
            &mut state,
            None,
        );
        assert!(matches!(result, Err(interp::InterpError::Undefined(_))));
        assert_eq!(state.memory.read(p, 4).unwrap(), 15u32.to_le_bytes());
        tir::verify_op_tree(&c, module.id()).unwrap();
    }
}

#[test]
fn private_integer_cells_preserve_returned_bits_poison_and_external_memory() {
    use tir::{AnalysisManager, Pass};

    enum Law {
        Product,
        Last,
        Sum,
        Offset,
        Double,
        Distinct,
        Interior,
    }
    let chain = r#"module {
  %f = func.func @f(%0: !i32, %1: !i32) -> !i32 {
    %2 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    ptr.store %0, %2
    %3 = ptr.load %2 : !i32
    ptr.store %3, %2
    %4 = ptr.load %2 : !i32
    ptr.store %1, %2
    %5 = ptr.load %2 : !i32
    %6 = addi %4, %5 : !i32
    func.return %6
  }
  module_end
}"#;
    for (original, law) in [
        (
            include_str!("../../../../core/checks/Inputs/eqsat/slots_straight_line.tir"),
            Law::Product,
        ),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/slot_last_store_promote.tir"),
            Law::Last,
        ),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/dse_distinct_allocas_no_alias.tir"),
            Law::Distinct,
        ),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/dse_ptradd_chains_one_object.tir"),
            Law::Interior,
        ),
        (chain, Law::Sum),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/nodes-fold.tir"),
            Law::Offset,
        ),
        (
            include_str!("../../../../core/checks/Inputs/eqsat/nodes-load-forward.tir"),
            Law::Double,
        ),
    ] {
        for width in [8, 16, 32, 64] {
            if matches!(law, Law::Distinct | Law::Interior) && width != 32 {
                continue;
            }
            let source = original.replace("!i32", &format!("!i{width}")).replace(
                "size = 4, align = 4",
                &format!("size = {}, align = {}", width / 8, width / 8),
            );
            let (c, f, _, _) = fixture(&source);
            let mut pass = tir::passes::EqSatPass::new();
            for round in 0..=2 {
                if round > 0 {
                    pass.run(f, &c, &AnalysisManager::new()).unwrap();
                    tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
                    let output = printed_memory(&c, f);
                    for operation in ["ptr.alloca", "ptr.store", "ptr.load"] {
                        assert!(!output.contains(operation), "{output}");
                    }
                    if width == 32
                        && matches!(
                            law,
                            Law::Offset | Law::Double | Law::Distinct | Law::Interior
                        )
                    {
                        filecheck::verify(
                            &filecheck::Source::new("original", original),
                            &filecheck::Source::new("optimized", &output),
                            &filecheck::Config::default(),
                        )
                        .unwrap();
                    }
                }
                let values = [0, 1, u64::MAX, 1 << (width - 1), 0x0123_4567_89ab_cdef];
                let ys = if matches!(law, Law::Offset | Law::Double) {
                    &[Some(0)][..]
                } else {
                    &[Some(0), Some(1), Some(u64::MAX), None][..]
                };
                for x in values.map(Some).into_iter().chain([None]) {
                    for &y in ys {
                        let value = |bits: Option<u64>| {
                            bits.map_or(Value::Poison, |bits| Value::Int(APInt::new(width, bits)))
                        };
                        let mut state = ExecutionState::default();
                        let external = state.memory.alloc(8, 1);
                        state.memory.write(external, &[0xa5; 8]).unwrap();
                        let actual = interp::run_function_in_state(
                            &c,
                            f,
                            if matches!(law, Law::Offset | Law::Double) {
                                vec![value(x)]
                            } else if matches!(law, Law::Distinct) {
                                vec![Value::Poison, value(x), value(y)]
                            } else {
                                vec![value(x), value(y)]
                            },
                            &mut state,
                            None,
                        )
                        .unwrap();
                        let expected = match law {
                            Law::Product => {
                                x.zip(y).map(|(x, y)| x.wrapping_mul(y).wrapping_add(1))
                            }
                            Law::Sum | Law::Distinct => x.zip(y).map(|(x, y)| x.wrapping_add(y)),
                            Law::Last | Law::Interior => y,
                            Law::Offset => x.map(|x| x.wrapping_add(6)),
                            Law::Double => x.map(|x| x.wrapping_mul(2)),
                        };
                        match (actual.as_slice(), expected) {
                            ([Value::Int(actual)], Some(bits)) => {
                                assert_eq!(*actual, APInt::new(width, bits))
                            }
                            ([Value::Poison], None) => {}
                            _ => panic!("wrong i{width} private-cell result: {actual:?}"),
                        }
                        assert_eq!(state.memory.read(external, 8).unwrap(), [0xa5; 8]);
                    }
                }
            }
        }
    }
}

#[test]
fn private_integer_cells_keep_escaping_partial_and_unsupported_accesses() {
    use tir::{AnalysisManager, Pass};
    let original = include_str!("../../../../core/checks/Inputs/eqsat/slot_last_store_promote.tir");
    let interior =
        include_str!("../../../../core/checks/Inputs/eqsat/dse_ptradd_chains_one_object.tir");
    let pointer_escape = include_str!(
        "../../../../core/checks/Inputs/eqsat/slot_promote_through_alias.tir"
    )
    .replace(
        "@stored_pointer(%0: !i32, %1: !i32) -> !i32",
        "@stored_pointer(%0: !i32, %1: !i32) -> !ptr.p<!i32>",
    )
    .replace("    ptr.store %1, %4\n", "")
    .replace("    %5 = ptr.load %2 : !i32\n", "")
    .replace("func.return %5", "func.return %4")
    .replace(
        "    %4 = ptr.load %3",
        "    %retained = constant {value = 1, retained = true} : !i32\n    %4 = ptr.load %3",
    );
    for (source, keeps_load) in [
        (pointer_escape, true),
        (interior.replace("size = 8", "size = 7"), false),
        (interior.replace("value = 4", "value = -4").replace("value = 2", "value = -2"), false),
        (interior.replace("    %8 = ptr.load", "    ptr.store %0, %6\n    %8 = ptr.load"), false),
        (interior.replace("    %8 = ptr.load", "    ptr.store %0, %2\n    %8 = ptr.load"), false),
        (interior.replace("    func.return %8", "    %delta = ptr.ptrdiff %6, %2 : !i64\n    %low = trunci %delta : !i32\n    %sum = addi %8, %low : !i32\n    func.return %sum"), false),
        (interior.replace("size = 8, align = 8", "size = 8, align = 8, unknown_contract = true"), false),
        (interior.replace("ptr.ptradd %2, %3 :", "ptr.ptradd %2, %3 {unknown_contract = true} :"), false),
        (interior.replace("ptr.store %1, %7", "ptr.store %1, %7 {volatile = true}"), false),
        (interior.replace("ptr.load %4 :", "ptr.load %4 {volatile = true} :"), true),
        (original.replace("    ptr.store %0, %2\n    ptr.store %1, %2\n", ""), true),
        (original
            .replace("ptr.load %2 : !i32", "ptr.load %2 : !i16")
            .replace("-> !i32", "-> !i16"), true),
        (original.replace("%1: !i32", "%1: !i16"), true),
        (original.replace("ptr.load %2 :", "ptr.load %2 {unknown_contract = true} :"), true),
        (original.replace("ptr.store %1, %2", "ptr.store %1, %2 {volatile = true}"), true),
        (original.replace(
            "    func.return %3",
            "    %escape = ptr.cmp %2, %2 {predicate = \"eq\"} : !i1\n    func.return %3",
        ), false),
        (original.replace("    func.return %3", "    cfg.br ^next\n  ^next:\n    %two = constant {value = 2} : !i32\n    %scaled = muli %3, %two : !i32\n    func.return %scaled"), true),
        (original.replace(
            "size = 4, align = 4",
            "size = 4, align = 4, unknown_contract = true",
        ), false),

    ] {
        let (c, f, _, _) = fixture(&source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        assert!(printed_memory(&c, f).contains("ptr.alloca"), "{source}");
        if keeps_load {
            assert!(printed_memory(&c, f).contains("ptr.load"), "{source}");
        }
    }
}

#[test]
fn private_integer_cells_keep_address_and_result_metadata_references() {
    use tir::{AnalysisManager, Pass};
    let source = include_str!("../../../../core/checks/Inputs/eqsat/slot_last_store_promote.tir");
    let interior =
        include_str!("../../../../core/checks/Inputs/eqsat/dse_ptradd_chains_one_object.tir");
    let constant = interior
        .replace(
            "    %3 = constant",
            "    %stored = constant {value = 37} : !i32\n    %3 = constant",
        )
        .replace("ptr.store %0, %4", "ptr.store %stored, %4")
        .replace("ptr.store %1, %7", "ptr.store %stored, %7");
    for (source, index, result) in [
        (source, 0, false),
        (source, 3, true),
        (interior, 0, false),
        (interior, 5, false),
        (interior, 2, false),
        (constant.as_str(), 0, false),
    ] {
        let (c, f, _, _) = fixture(source);
        let body = c.get_region(c.get_op(f).regions()[0]);
        let ops = body.iter(c.clone()).next().unwrap().op_ids();
        let value = c.get_op(ops[index]).results()[0];
        let ret = *ops.last().unwrap();
        let mut attrs = c.get_op(ret).attributes();
        attrs.push(c.named_attribute(
            "keep",
            tir::attributes::AttributeValue::Array(
                vec![tir::attributes::AttributeValue::Value(value)].into(),
            ),
        ));
        c.set_op_attributes(ret, attrs);
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        assert!(printed_memory(&c, f).contains("ptr.alloca"));
        if result {
            assert!(printed_memory(&c, f).contains("ptr.load"));
        }
        assert!(c.get_value(value).defining_op().is_some());
    }
}

#[test]
fn undersized_private_cells_preserve_the_access_fault() {
    use tir::{AnalysisManager, Pass};
    let source = include_str!("../../../../core/checks/Inputs/eqsat/slot_last_store_promote.tir")
        .replace("size = 4", "size = 1");
    let (c, f, _, _) = fixture(&source);
    for round in 0..=1 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
        }
        for x in [Value::Int(APInt::new(32, 0)), Value::Poison] {
            let result = interp::run_function(&c, f, vec![x, Value::Int(APInt::new(32, 1))]);
            assert!(matches!(
                result,
                Err(interp::InterpError::OutOfBounds { .. })
            ));
        }
    }
}

#[test]
fn private_cell_groups_preserve_escaping_addresses_and_foreign_access_faults() {
    use tir::{AnalysisManager, Pass};
    for source in [
        r#"module {
  %f = func.func @main() -> !ptr.p<!i32> {
    %cell = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    %x = constant {value = 7} : !i32
    ptr.store %x, %cell
    %read = ptr.load %cell : !i32
    %escaped = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    func.return %escaped
  }
  module_end
}
"#,
        r#"module {
  %f = func.func @main() -> !i64 {
    %foreign_cell = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    %cell = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    %x = constant {value = 7} : !i32
    ptr.store %x, %cell
    %read = ptr.load %cell : !i32
    %foreign = ptr.load %foreign_cell : !i64
    func.return %foreign
  }
  module_end
}
"#,
    ] {
        let (c, f, _, _) = fixture(source);
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        let expected = interp::run_function(&c, f, vec![]).unwrap();
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        let actual = interp::run_function(&c, f, vec![]).unwrap();
        match (expected.as_slice(), actual.as_slice()) {
            ([Value::Ptr(expected)], [Value::Ptr(actual)]) => assert_eq!(actual, expected),
            ([Value::Poison], [Value::Poison]) => {}
            _ => panic!("private-cell group changed {expected:?} into {actual:?}"),
        }
    }
}

#[test]
fn ordinary_adjacent_reads_keep_faults_poison_bytes_and_state_demand() {
    use tir::{AnalysisManager, Pass};
    let explicit = include_str!("../../../../core/checks/Inputs/eqsat/load_cse_via_state.tir");
    let implicit = include_str!("../../../../core/checks/Inputs/eqsat/cse_keeps_the_fork_join.tir");
    for (source, loads, metadata) in [
        (explicit.to_owned(), 1, false),
        (implicit.to_owned(), 1, false),
        (explicit.replace("%5 = subi %2, %4", "%5 = subi %4, %4"), 1, false),
        (explicit.replace("%4 = ptr.load", "%4, %last = ptr.load").replace("func.return %5", "%write = ptr.store %5, %0 state(%last)\n    func.return %5"), 2, false),
        (explicit.replace("%4 = ptr.load", "%4, %last = ptr.load"), 2, false),
        (explicit.replace("%4 = ptr.load %0 state(%3)", "%4 = ptr.load %0 state(%1)"), 2, false),
        (explicit.replace("state(%3) : !i32", "state(%3) {volatile = true} : !i32"), 2, false),
        (implicit.replace("%1 = ptr.load %p : !i32", "%1 = ptr.load %p {atomic = true} : !i32"), 2, false),
        (implicit.replace("%0 = ptr.load %p : !i32", "%0 = ptr.load %p {unknown_contract = true} : !i32"), 2, false),
        (implicit.replace("%1 = ptr.load %p", "%1 = ptr.load %q"), 2, false),
        (implicit.replace("%1 = ptr.load", "ptr.store %0, %p\n    %1 = ptr.load"), 1, false),
        (implicit.replace("%2 = addi %0, %1", "cfg.br ^next\n  ^next:\n    %2 = addi %0, %1"), 2, false),
        (implicit.replace("module {", "module {layout.spec = #layout.spec<endianness = \"big\", !ptr.p = {size = 64, abi = 64}>} {"), 2, false),
        (implicit.replace("%2 = addi %0, %1 : !i32", "%cond = cmpi %0, %1 {predicate = \"eq\"} : !i1\n    %2 = scf.switch %cond args(%0) (%carry) {\n      %sum = addi %carry, %1 : !i32\n      -> %sum\n    }\n    (%other) {\n      -> %1\n    }"), 2, false),
        (implicit.to_owned(), 2, true),
    ] {
        let (c, f, _, _) = fixture(&source);
        if metadata {
            let body = c.get_region(c.get_op(f).regions()[0]);
            let ops = body.iter(c.clone()).next().unwrap().op_ids();
            let value = c.get_op(ops[1]).results()[0];
            let ret = *ops.last().unwrap();
            let mut attributes = c.get_op(ret).attributes();
            attributes.push(c.named_attribute("keep", tir::attributes::AttributeValue::Value(value)));
            c.set_op_attributes(ret, attributes);
        }
        let observe = || {
            let mut observations = Vec::new();
            for initialized in [false, true] {
                for seed in [0, 1, 0xa5, 0xff] {
                    for offset in [Some(0), Some(1), Some(4), Some(5), Some(8), None] {
                        let mut state = ExecutionState::default();
                        let external = state.memory.alloc(4, 1);
                        state.memory.write(external, &[0xa5; 4]).unwrap();
                        let data = state.memory.alloc(8, 1);
                        if initialized { state.memory.write(data, &[seed; 8]).unwrap(); }
                        let mut arguments = vec![offset.map_or(Value::Poison, |o| Value::Ptr(data + o))];
                        if c.get_region(c.get_op(f).regions()[0]).ports().len() == 2 {
                            arguments.push(Value::Poison);
                        }
                        let actual = interp::run_function_in_state(&c, f, arguments, &mut state, Some(256));
                        let result = match actual {
                            Ok(values) => format!("{:?}", values.into_iter().map(|v| match v {
                                Value::Int(bits) => Value::Int(bits.with_signed(false)), other => other,
                            }).collect::<Vec<_>>()),
                            Err(error) => format!("{error:?}"),
                        };
                        let bytes = (0..12).map(|i| format!("{:?}", state.memory.read(external + i, 1))).collect::<Vec<_>>();
                        observations.push((result, bytes));
                    }
                }
            }
            observations
        };
        let before = observe();
        for _ in 0..2 {
            tir::passes::EqSatPass::new().run(f, &c, &AnalysisManager::new()).unwrap();
            tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
            assert_eq!(printed_memory(&c, f).matches("ptr.load").count(), loads, "{source}");
            assert_eq!(observe(), before, "{source}");
        }
    }
}

#[test]
fn disjoint_private_reads_preserve_bounds_faults_poison_and_bytes() {
    let original =
        include_str!("../../../../core/checks/Inputs/eqsat/dse_read_fork_off_a_read.tir");
    for (source, stores, loads) in [
        (original.to_owned(), 2, 1),
        (original.replace("{value = 4}", "{value = 2}"), 3, 1),
        (original.replace("{value = 4}", "{value = 5}"), 3, 1),
        (original.replace("{value = 4}", "{value = -1}"), 3, 1),
        (original.replace("%1: !i32", "%1: !i16"), 3, 1),
        (
            original.replace("size = 8, align = 8", "size = 4, align = 8"),
            3,
            1,
        ),
        (
            original.replace(
                "    %5 = constant",
                "    %partial = divsi %1, %2 : !i32\n    %5 = constant",
            ),
            3,
            1,
        ),
        (
            original.replace(
                "    ptr.store %2, %3",
                "    ptr.store %1, %6\n    ptr.store %2, %3",
            ),
            4,
            1,
        ),
        (
            original.replace("%7 = ptr.load %6 :", "%7 = ptr.load %6 {volatile = true} :"),
            3,
            2,
        ),
        (
            original.replace(
                "ptr.ptradd %3, %5 :",
                "ptr.ptradd %3, %5 {unknown_contract = true} :",
            ),
            3,
            1,
        ),
        (
            original.replace(
                "size = 8, align = 8",
                "size = 8, align = 8, unknown_contract = true",
            ),
            3,
            1,
        ),
        (
            original.replace("ptr.store %2, %3", "ptr.store %2, %3 {atomic = true}"),
            3,
            1,
        ),
        (
            original.replace(
                "#layout.spec<!i32",
                "#layout.spec<endianness = \"big\", !i32",
            ),
            3,
            3,
        ),
    ] {
        disjoint_private_read_case(&source, stores, loads);
    }
}

fn disjoint_private_read_case(source: &str, stores: usize, loads: usize) {
    use tir::{AnalysisManager, Pass};
    let (c, f, _, _) = fixture(source);
    let types = c
        .get_region(c.get_op(f).regions()[0])
        .ports()
        .iter()
        .map(tir::Value::ty)
        .collect::<Vec<_>>();
    let observe = || {
        let mut observations = Vec::new();
        for poison in 0..=types.len() {
            for seed in [0_i64, -1, -2, 0x1234_5678_9abc_def0] {
                let mut state = ExecutionState::default();
                state.fp_environment.flags = 0x10;
                let external = state.memory.alloc(8, 8);
                state.memory.write(external, &[0xa5; 8]).unwrap();
                let args = types
                    .iter()
                    .enumerate()
                    .map(|(i, &ty)| {
                        if i == poison {
                            Value::Poison
                        } else {
                            let width = tir::builtin::IntegerType::view(&c, ty).unwrap().width();
                            Value::Int(tir::utils::APInt::new_signed(
                                width,
                                seed.wrapping_add(i as i64),
                            ))
                        }
                    })
                    .collect();
                let result = interp::run_function_in_state(&c, f, args, &mut state, Some(256));
                let result = match result {
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
                    Err(error) => format!("{error:?}"),
                };
                let bytes = (0..16)
                    .map(|i| format!("{:?}", state.memory.read(external + i, 1)))
                    .collect::<Vec<_>>();
                observations.push((result, bytes, state.fp_environment));
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
        assert_eq!(output.matches("ptr.store").count(), stores, "{source}");
        assert_eq!(output.matches("ptr.load").count(), loads, "{source}");
        assert_eq!(observe(), before, "{source}");
    }
}

mod gamma_cells;

mod theta_cells;

mod store_demand;

mod theta_cohort;

mod pointer_addresses;

mod ordinary_layouts;

mod ordered_cells;

mod implicit_overwrites;

#[test]
fn constant_shift_stores_preserve_poison_and_the_last_write() {
    use tir::{AnalysisManager, Pass};
    let original =
        include_str!("../../../../core/checks/Inputs/eqsat/dse_late_value_congruence.tir");
    for invalid in [false, true] {
        let source = if invalid {
            original.replace("value = 12", "value = 32")
        } else {
            original.to_owned()
        };
        let (c, f, _, _) = fixture(&source);
        for round in 0..=2 {
            if round > 0 {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
                if !invalid {
                    let output = printed_memory(&c, f);
                    filecheck::verify(
                        &filecheck::Source::new("constant shift stores", original),
                        &filecheck::Source::new("optimized", &output),
                        &filecheck::Config::default(),
                    )
                    .unwrap();
                    assert_eq!(output.matches("ptr.store").count(), 1, "{output}");
                    assert!(!output.contains("shrsi"), "{output}");
                }
            }
            for pointer in 0..3 {
                let mut state = ExecutionState::default();
                let base = state.memory.alloc(8, 1);
                state.memory.write(base, &[0xa5; 8]).unwrap();
                let address = match pointer {
                    0 => Value::Ptr(base + 2),
                    1 => Value::Ptr(base + 7),
                    _ => Value::Poison,
                };
                let result =
                    interp::run_function_in_state(&c, f, vec![address], &mut state, Some(128));
                match pointer {
                    0 => {
                        assert!(result.unwrap().is_empty());
                        assert_eq!(state.memory.read(base, 2).unwrap(), [0xa5; 2]);
                        assert_eq!(state.memory.read(base + 6, 2).unwrap(), [0xa5; 2]);
                        if invalid {
                            assert!(matches!(
                                state.memory.read(base + 2, 4),
                                Err(interp::InterpError::Uninitialized { size: 4, .. })
                            ));
                        } else {
                            assert_eq!(state.memory.read(base + 2, 4).unwrap(), [0; 4]);
                        }
                    }
                    1 => {
                        assert!(matches!(
                            result,
                            Err(interp::InterpError::OutOfBounds { size: 4, .. })
                        ));
                        assert_eq!(state.memory.read(base, 8).unwrap(), [0xa5; 8]);
                    }
                    _ => {
                        assert!(matches!(result, Err(interp::InterpError::Undefined(_))));
                        assert_eq!(state.memory.read(base, 8).unwrap(), [0xa5; 8]);
                    }
                }
            }
        }
    }
}

#[test]
fn private_cells_across_fp_division_preserve_flags_traps_poison_and_external_reads() {
    use tir::{AnalysisManager, Pass};
    let original =
        include_str!("../../../../core/checks/Inputs/eqsat/fp-private-slot-across-trap.tir");
    for external_read in [false, true] {
        let source = if external_read {
            original
                .replace("%b: !f64)", "%b: !f64, %outside: !ptr.p)")
                .replace(
                    "    %env =",
                    "    %observed = ptr.load %outside : !i32\n    %env =",
                )
        } else {
            original.to_owned()
        };
        let (c, f, _, _) = fixture(&source);
        let division = c
            .get_region(c.get_op(f).regions()[0])
            .op_ids()
            .into_iter()
            .find(|&id| c.get_op(id).is_operation(&tir::fp::ops::DIV))
            .unwrap();
        let original_division = {
            let op = c.get_op(division);
            (op.operands(), op.results(), op.attributes())
        };
        for round in 0..=2 {
            if round > 0 {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &AnalysisManager::new())
                    .unwrap();
                tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
                let op = c.get_op(division);
                assert_eq!(
                    (op.operands(), op.results(), op.attributes()),
                    original_division
                );
                let output = printed_memory(&c, f);
                assert_eq!(output.matches(" = fp.div ").count(), 1, "{output}");
                if external_read {
                    assert!(
                        output.contains("ptr.alloca")
                            && output.contains("ptr.store")
                            && output.contains("ptr.load"),
                        "{output}"
                    );
                } else {
                    filecheck::verify(
                        &filecheck::Source::new("FP private cell", original),
                        &filecheck::Source::new("optimized", &output),
                        &filecheck::Config::default(),
                    )
                    .unwrap();
                    for operation in ["ptr.alloca", "ptr.store", "ptr.load"] {
                        assert!(!output.contains(operation), "{output}");
                    }
                }
            }
            fp_private_cell_observations(&c, f, external_read);
        }
    }
}

fn fp_private_cell_observations(c: &Context, f: OpId, external_read: bool) {
    use tir::utils::APFloat;
    for (a, b, raised) in [(6.0, 2.0, 0), (1.0, 0.0, 8), (0.0, 0.0, 16), (1.0, 3.0, 1)] {
        for traps in [0, 31] {
            for initial_flags in [0, 4] {
                for first in [Value::Int(APInt::new(32, 7)), Value::Poison] {
                    for last in [Value::Int(APInt::new(32, 13)), Value::Poison] {
                        let mut state = ExecutionState::default();
                        let external = state.memory.alloc(4, 4);
                        state.memory.write(external, &[27, 52, 121, 255]).unwrap();
                        state.fp_environment.flags = initial_flags;
                        state.fp_environment.traps = traps;
                        let mut inputs = vec![
                            first.clone(),
                            last.clone(),
                            Value::Float(APFloat::from_f64(a)),
                            Value::Float(APFloat::from_f64(b)),
                        ];
                        if external_read {
                            inputs.push(Value::Ptr(external));
                        }
                        let actual =
                            interp::run_function_in_state(c, f, inputs, &mut state, Some(256));
                        assert_eq!(state.fp_environment.flags, initial_flags | raised);
                        assert_eq!(state.fp_environment.traps, traps);
                        if raised & traps == 0 {
                            assert_eq!(
                                format!("{actual:?}"),
                                format!("Ok({:?})", vec![last.clone()])
                            );
                        } else {
                            assert_eq!(actual.unwrap_err().to_string(), "floating-point trap");
                        }
                        assert_eq!(state.memory.read(external, 4).unwrap(), [27, 52, 121, 255]);
                    }
                }
            }
        }
    }
}

#[test]
fn constant_cfg_cells_preserve_loop_demand_and_faults() {
    let source = include_str!("../../../../core/checks/Inputs/eqsat/nodes-loop-invariant.tir");
    for n in [0, 1, 2, 4, 0x8000_0000, u32::MAX as u64] {
        assert_constant_cfg_cell(source, true, vec![Value::Int(APInt::new(32, n))], "14");
    }
    assert_constant_cfg_cell(
        source,
        true,
        vec![Value::Int(APInt::new(32, 0x7fff_ffff))],
        "step-limit",
    );
    assert_constant_cfg_cell(source, true, vec![Value::Poison], "undefined");
    let partial = source.replace(
        "%more = cmpi %next, %n",
        "%bad = divsi %one, %zero : !i32\n    %more = cmpi %next, %bad",
    );
    assert_constant_cfg_cell(
        &partial,
        true,
        vec![Value::Int(APInt::new(32, 4))],
        "undefined",
    );
    let entry_read = source
        .replace(
            "    cfg.br ^body",
            "    %early = ptr.load %p : !i32\n    cfg.br ^body",
        )
        .replace("%s = addi %r, %r", "%s = addi %r, %early");
    assert_constant_cfg_cell(&entry_read, true, vec![Value::Int(APInt::new(32, 4))], "14");
}

#[test]
fn constant_cfg_cells_require_closed_complete_initialization_and_readback() {
    let source = include_str!("../../../../core/checks/Inputs/eqsat/nodes-loop-invariant.tir");
    for (old, replacement, expected) in [
        (
            "{size = 4, align = 4}",
            "{size = 4, align = 4, tag = \"keep\"}",
            "14",
        ),
        (
            "%v = ptr.load %p : !i32",
            "%v = ptr.load %p {tag = \"keep\"} : !i32",
            "14",
        ),
        (
            "ptr.store %w, %p",
            "ptr.store %w, %p {tag = \"keep\"}",
            "14",
        ),
        (
            "ptr.store %seven, %p",
            "ptr.store %seven, %p {tag = \"keep\"}",
            "14",
        ),
        (
            "ptr.store %w, %p",
            "ptr.store %w, %p\n    ptr.store %one, %p",
            "2",
        ),
        (
            "addi %v, %zero : !i32",
            "addi %v, %zero {nsw = true} : !i32",
            "14",
        ),
        ("{size = 4, align = 4}", "{size = 8, align = 4}", "14"),
        ("    ptr.store %seven, %p\n", "", "poison"),
        (
            "    ptr.store %seven, %p",
            "    %byte = constant {value = 7} : !i8\n    ptr.store %byte, %p",
            "poison",
        ),
        (
            "    ptr.store %w, %p",
            "    %bad = divui %one, %zero : !i32\n    ptr.store %w, %p",
            "undefined",
        ),
        (
            "%v = ptr.load %p : !i32",
            "%wide = ptr.load %p : !i64\n    %v = trunci %wide : !i32",
            "bounds",
        ),
        (
            "module {",
            "module {layout.spec = #layout.spec<endianness = \"big\">} {",
            "14",
        ),
        (
            "    %zero =",
            "    %other = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>\n    %zero =",
            "14",
        ),
    ] {
        assert_constant_cfg_cell(
            &source.replace(old, replacement),
            false,
            vec![Value::Int(APInt::new(32, 4))],
            expected,
        );
    }
    let call = source
        .replace("    %next =", "    %call = func.call %fn_helper() -> !i32\n    %next =")
        .replace("  module_end", "  %fn_helper = func.func @helper() -> !i32 {\n    %h = constant {value = 1} : !i32\n    func.return %h\n  }\n  module_end");
    assert_constant_cfg_cell(&call, false, vec![Value::Int(APInt::new(32, 4))], "14");
    let read_before_initialization = source
        .replace(
            "    ptr.store %seven, %p",
            "    %early = ptr.load %p : !i32\n    ptr.store %seven, %p",
        )
        .replace("%s = addi %r, %r", "%s = addi %r, %early");
    assert_constant_cfg_cell(
        &read_before_initialization,
        false,
        vec![Value::Int(APInt::new(32, 4))],
        "poison",
    );
    let partial_init = source.replace(
        "    ptr.store %seven, %p",
        "    %byte = constant {value = 7} : !i8\n    ptr.store %byte, %p",
    );
    let narrow = partial_init.replace(
        "%r = ptr.load %p : !i32",
        "%low = ptr.load %p : !i8\n    %r = extui %low : !i32",
    );
    assert_constant_cfg_cell(
        &narrow,
        false,
        vec![Value::Int(APInt::new(32, 4))],
        "poison",
    );
    let unknown = source
        .replace("@f(%n: !i32)", "@f(%n: !i32, %seed: !i32)")
        .replace("ptr.store %seven, %p", "ptr.store %seed, %p");
    for (seed, expected) in [
        (Value::Int(APInt::new(32, 8)), "16"),
        (Value::Poison, "poison"),
    ] {
        assert_constant_cfg_cell(
            &unknown,
            false,
            vec![Value::Int(APInt::new(32, 4)), seed],
            expected,
        );
    }
    let foreign = source
        .replace("@f(%n: !i32)", "@f(%n: !i32, %external: !ptr.p<!i32>)")
        .replace(
            "    %next =",
            "    %fault = ptr.load %external : !i32\n    %next =",
        );
    assert_constant_cfg_cell(
        &foreign,
        false,
        vec![Value::Int(APInt::new(32, 4)), Value::Ptr(u64::MAX)],
        "bounds",
    );
    let external = source
        .replace("@f(%n: !i32)", "@f(%n: !i32, %external: !ptr.p<!i32>)")
        .replace("    %next =", "    ptr.store %one, %external\n    %next =");
    assert_constant_cfg_cell(
        &external,
        false,
        vec![Value::Int(APInt::new(32, 4)), Value::Ptr(0)],
        "14",
    );
}

fn assert_constant_cfg_cell(source: &str, promoted: bool, arguments: Vec<Value>, expected: &str) {
    use tir::{AnalysisManager, Pass};
    let (c, f, _, _) = fixture(source);
    for round in 0..=2 {
        if round > 0 {
            tir::passes::EqSatPass::new()
                .run(f, &c, &AnalysisManager::new())
                .unwrap();
            let output = printed_memory(&c, f);
            assert_eq!(!output.contains("ptr."), promoted, "{source}\n{output}");
            assert!(
                output.contains("cfg.br") && output.contains("cfg.cond_br"),
                "{output}"
            );
            if promoted {
                filecheck::verify(
                    &filecheck::Source::new(
                        "constant CFG cell",
                        include_str!(
                            "../../../../core/checks/Inputs/eqsat/nodes-loop-invariant.tir"
                        ),
                    ),
                    &filecheck::Source::new("optimized", &output),
                    &filecheck::Config::default(),
                )
                .unwrap();
            }
        }
        tir::verify_op_tree(&c, c.parent_op(f).unwrap()).unwrap();
        let mut state = ExecutionState::default();
        let external = state.memory.alloc(8, 1);
        state.memory.write(external, &[0x5a; 8]).unwrap();
        let writes_external = arguments.iter().any(|value| matches!(value, Value::Ptr(0)));
        let inputs = arguments
            .iter()
            .map(|value| {
                if matches!(value, Value::Ptr(0)) {
                    Value::Ptr(external)
                } else {
                    value.clone()
                }
            })
            .collect();
        let result = interp::run_function_in_state(&c, f, inputs, &mut state, Some(256));
        let actual = match result {
            Ok(values) => match &values[..] {
                [Value::Int(value)] => value.to_u64().to_string(),
                [Value::Poison] => "poison".to_owned(),
                values => panic!("unexpected result {values:?}"),
            },
            Err(interp::InterpError::Undefined(_)) => "undefined".to_owned(),
            Err(interp::InterpError::StepLimit) => "step-limit".to_owned(),
            Err(interp::InterpError::OutOfBounds { .. }) => "bounds".to_owned(),
            error => panic!("unexpected result {error:?}"),
        };
        assert_eq!(actual, expected, "round={round}\n{source}");
        assert_eq!(
            state.memory.read(external, 8).unwrap(),
            if writes_external {
                [1, 0, 0, 0, 0x5a, 0x5a, 0x5a, 0x5a]
            } else {
                [0x5a; 8]
            }
        );
    }
}

#[test]
fn disjoint_parameter_reads_keep_the_write_before_a_bounds_fault() {
    use tir::{AnalysisManager, Pass};
    let source = include_str!("../../../../core/checks/Inputs/eqsat/dse_disjoint_read_fork.tir");
    let c = Context::with_default_dialects();
    let module = tir::parse::ir::parse_operation(&c, source, &MODULE).unwrap();
    for (name, fault_offset) in [("fork", 8_i64), ("before", -4), ("sequential", 8)] {
        let function = c
            .get_region(module.regions()[0])
            .op_ids()
            .into_iter()
            .find(|&id| {
                c.get_op(id).is_operation(&tir::func::FUNC)
                    && c.get_op(id)
                        .clone()
                        .as_interface::<dyn tir::Symbol>()
                        .is_some_and(|symbol| symbol.symbol_name() == name)
            })
            .unwrap();
        for optimize in [false, true] {
            if optimize {
                tir::passes::EqSatPass::new()
                    .run(function, &c, &AnalysisManager::new())
                    .unwrap();
            }
            tir::verify_op_tree(&c, module.id()).unwrap();
            let mut state = ExecutionState::default();
            let base = state.memory.alloc(8, 1);
            state.memory.write(base, &[0xa5; 8]).unwrap();
            let result = interp::run_function_in_state(
                &c,
                function,
                vec![
                    Value::Ptr(base),
                    Value::Int(APInt::new(32, 0x1122_3344)),
                    Value::Int(APInt::new(32, 0x5566_7788)),
                ],
                &mut state,
                Some(128),
            );
            assert!(
                matches!(result, Err(interp::InterpError::OutOfBounds { address, size: 4 })
                    if address == base.wrapping_add_signed(fault_offset)),
                "{name}, optimize={optimize}: {result:?}"
            );
            assert_eq!(
                state.memory.read(base, 8).unwrap(),
                [0x44, 0x33, 0x22, 0x11, 0xa5, 0xa5, 0xa5, 0xa5],
                "{name}, optimize={optimize}"
            );
        }
    }
}

mod opaque_calls {
    use super::*;
    use tir::{AnalysisManager, Pass};

    tir::helpers::op_def! {
        file: "tests/inputs/eqsat-opaque-memory.cdsl",
        hosts: {
            checksum_interp => (tir::interp::Interp, &tir::interp::InterpCallbacks {
                evaluate: |operation, _operands, state| {
                    let start = interp::Memory::default().alloc(0, 1);
                    let end = state.memory.alloc(0, 1);
                    let checksum = (start..end)
                        .filter_map(|address| state.memory.read(address, 1).ok())
                        .fold(0u8, |sum, bytes| sum.wrapping_add(bytes[0]));
                    for address in start..end {
                        state.memory.write(address, &[checksum])?;
                    }
                    if operation.attr("stop")
                        == Some(tir::attributes::AttributeValue::Bool(true))
                    {
                        return Err(interp::InterpError::Message("opaque sink stop".into()));
                    }
                    Ok(Vec::new())
                },
            }),
            checksum_effects => (tir::ResourceEffects, &tir::ResourceEffectsCallbacks {
                resource_effects: |_operation| vec![tir::ResourceEffect {
                    resource: tir::builtin::StateResource::Memory,
                    access: tir::ResourceAccess::Change,
                    observed: vec![],
                    produced: vec![],
                }],
            }),
        },
    }
    tir::helpers::dialect! {
        OpaqueMemoryDialect {
            name: "eqsat_opaque_test",
            operations: [CHECKSUM_MEMORY],
            types: [],
        }
    }

    #[test]
    fn opaque_calls_observe_private_cells_without_pointer_arguments() {
        let fixtures = [
            include_str!(
                "../../../../core/checks/Inputs/eqsat/dse_private_slot_across_call_dies.tir"
            ),
            include_str!(
                "../../../../core/checks/Inputs/eqsat/nodes-forwarded-load-across-call.tir"
            ),
            include_str!("../../../../core/checks/Inputs/eqsat/slot_promote_through_ptradd.tir"),
        ];
        for (shape, fixture) in fixtures.into_iter().enumerate() {
            let source = fixture
                .replace(
                    "%fn_g = func.declare @g() -> !unit",
                    "%fn_g = func.func @g() {\n    eqsat_opaque_test.checksum_memory\n    func.return\n  }",
                )
                .replace(
                    "%fn_f = func.declare @f(!i8) -> !unit",
                    "%fn_f = func.func @f(%byte: !i8) { func.return }",
                );
            let elided = if shape == 1 {
                source
                    .lines()
                    .filter(|line| {
                        !line.contains("%slot = ptr.alloca")
                            && !line.contains("%w_slot = ptr.store")
                            && !line.contains("%back, %r_slot = ptr.load")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .replace("%fn_f(%back : !i8)", "%fn_f(%byte : !i8)")
                    .replace("state(%r_slot,", "state(%s_slot,")
            } else {
                source
                    .lines()
                    .filter(|line| {
                        ![
                            "ptr.alloca",
                            "ptr.store",
                            "ptr.load",
                            "ptr.ptradd",
                            "constant",
                        ]
                        .iter()
                        .any(|name| line.contains(name))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .replace("func.return %3", "func.return %1")
                    .replace("func.return %4", "func.return %0")
            };
            for (erased, program) in [(false, source), (true, elided)] {
                let c = Context::with_default_dialects();
                c.register_dialect::<OpaqueMemoryDialect>();
                let module = tir::parse::ir::parse_operation(&c, &program, &MODULE).unwrap();
                let f = c
                    .get_region(module.regions()[0])
                    .op_ids()
                    .into_iter()
                    .rfind(|&id| c.get_op(id).is_operation(&tir::func::FUNC))
                    .unwrap();
                for optimized in [false, true] {
                    if optimized {
                        tir::passes::EqSatPass::new()
                            .run(f, &c, &AnalysisManager::new())
                            .unwrap();
                    }
                    tir::verify_op_tree(&c, module.id()).unwrap();
                    let mut state = ExecutionState::default();
                    let external = state.memory.alloc(1, 1);
                    state.memory.write(external, &[99]).unwrap();
                    let mut checksum = 99u8;
                    let arguments = if shape == 1 {
                        let pointer = state.memory.alloc(8, 8);
                        let byte = state.memory.alloc(1, 1);
                        let address = byte.to_le_bytes();
                        state.memory.write(pointer, &address).unwrap();
                        state.memory.write(byte, &[73]).unwrap();
                        checksum = address
                            .iter()
                            .fold(99u8.wrapping_add(73), |sum, b| sum.wrapping_add(*b));
                        vec![Value::Ptr(pointer)]
                    } else if shape == 0 {
                        vec![
                            Value::Int(APInt::new(32, 73)),
                            Value::Int(APInt::new(32, 11)),
                        ]
                    } else {
                        vec![Value::Int(APInt::new(32, 73))]
                    };
                    if !erased {
                        checksum = checksum.wrapping_add(73);
                    }
                    let values =
                        interp::run_function_in_state(&c, f, arguments, &mut state, Some(128))
                            .unwrap();
                    assert_eq!(
                        state.memory.read(external, 1).unwrap(),
                        [checksum],
                        "shape={shape}, erased={erased}, optimized={optimized}"
                    );
                    let expected = match shape {
                        0 => vec![Some(11)],
                        1 => vec![],
                        2 => vec![Some(if erased {
                            73
                        } else {
                            i64::from(u32::from_le_bytes([checksum; 4]))
                        })],
                        _ => unreachable!(),
                    };
                    assert_eq!(
                        values.iter().map(Value::to_i64).collect::<Vec<_>>(),
                        expected
                    );
                }
            }
        }
    }

    #[test]
    fn nested_loop_cells_reach_the_first_opaque_sink() {
        let source = include_str!(
            "../../../../core/checks/Inputs/eqsat/theta_load_stored_two_gammas_deep.tir"
        )
        .replace(
            "%fn_sink = func.declare @sink(!i32) -> !i32",
            "%fn_sink = func.func @sink(%value: !i32) -> !i32 {\n    eqsat_opaque_test.checksum_memory {stop = true}\n    func.return %value\n  }",
        );
        let c = Context::with_default_dialects();
        c.register_dialect::<OpaqueMemoryDialect>();
        let module = tir::parse::ir::parse_operation(&c, &source, &MODULE).unwrap();
        let f = c
            .get_region(module.regions()[0])
            .op_ids()
            .into_iter()
            .rfind(|&id| c.get_op(id).is_operation(&tir::func::FUNC))
            .unwrap();
        for optimized in [false, true] {
            if optimized {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &AnalysisManager::new())
                    .unwrap();
            }
            tir::verify_op_tree(&c, module.id()).unwrap();
            for selector in [0, 1] {
                let mut state = ExecutionState::default();
                let external = state.memory.alloc(1, 1);
                state.memory.write(external, &[99]).unwrap();
                let actual = interp::run_function_in_state(
                    &c,
                    f,
                    vec![Value::Int(APInt::new(1, selector))],
                    &mut state,
                    Some(100),
                );
                if selector == 0 {
                    assert_eq!(
                        actual
                            .unwrap()
                            .iter()
                            .map(Value::to_i64)
                            .collect::<Vec<_>>(),
                        [Some(0)]
                    );
                } else {
                    assert!(
                        matches!(actual, Err(interp::InterpError::Message(message)) if message == "opaque sink stop")
                    );
                }
                assert_eq!(
                    state.memory.read(external, 1).unwrap(),
                    [if selector == 0 { 99 } else { 103 }],
                    "selector={selector}, optimized={optimized}"
                );
            }
        }
    }
}
