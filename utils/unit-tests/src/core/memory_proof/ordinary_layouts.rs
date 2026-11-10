use super::*;
use tir::{AnalysisManager, Pass};

const WRITE_ONLY: &str =
    include_str!("../../../../../core/checks/Inputs/eqsat/dse_write_only_slot_dies.tir");
const LE64: &str = "#layout.spec<endianness = \"little\", !ptr.p = {size = 64, abi = 64}>";
const BE64: &str = "#layout.spec<endianness = \"big\", !ptr.p = {size = 64, abi = 64}>";
const PTR32: &str = "#layout.spec<!ptr.p = {size = 32, abi = 32}>";

#[test]
fn original_write_only_loop_erases_memory_and_keeps_control() {
    let (c, f, _, _) = fixture(WRITE_ONLY);
    tir::passes::EqSatPass::new()
        .run(f, &c, &AnalysisManager::new())
        .unwrap();
    let output = printed_memory(&c, f);
    assert!(!output.contains("ptr.alloca"), "{output}");
    assert!(!output.contains("ptr.store"), "{output}");
    filecheck::verify(
        &filecheck::Source::new("write-only loop", WRITE_ONLY),
        &filecheck::Source::new("optimized", &output),
        &filecheck::Config::default(),
    )
    .unwrap();
    for (lo, hi) in [(0, 0), (5, 2), (-3, 2), (0, 5)] {
        let values = interp::run_function_within(
            &c,
            f,
            vec![
                Value::Int(APInt::new(32, lo as u32 as u64)),
                Value::Int(APInt::new(32, hi as u32 as u64)),
            ],
            Some(200),
        )
        .unwrap();
        assert!(values.is_empty());
    }
}

#[test]
fn write_only_loops_preserve_results_poison_and_partial_producers() {
    let finite = WRITE_ONLY
        .replace(
            "@f(%0: !i32, %1: !i32) {",
            "@f(%0: !i32, %1: !i32) -> !i32 {",
        )
        .replace("func.return\n", "func.return %4\n");
    let partial = finite.replace(
        "ptr.store %5, %2",
        "%zero = constant {value = 0} : !i32\n      %danger = divsi %5, %zero : !i32\n      ptr.store %danger, %2",
    );
    let conditional = finite
        .replace(
            "    %3 = constant",
            "    %test = cmpi %0, %1 {predicate = \"slt\"} : !i1\n    cfg.cond_br %test, ^go, ^stop\n  ^go:\n    %3 = constant",
        )
        .replace("    func.return %4", "    func.return %4\n  ^stop:\n    func.return %0");
    for (source, fault) in [(&finite, false), (&partial, true), (&conditional, false)] {
        let (c, f, _, _) = fixture(source);
        for optimized in [false, true] {
            if optimized {
                tir::passes::EqSatPass::new()
                    .run(f, &c, &AnalysisManager::new())
                    .unwrap();
                let output = printed_memory(&c, f);
                assert!(!output.contains("ptr.alloca"), "{output}");
                assert!(!output.contains("ptr.store"), "{output}");
                assert!(output.contains("scf.ordered_for"), "{output}");
                assert!(!fault || output.contains("divsi"), "{output}");
            }
            for (lo, hi) in [(0, 0), (5, 2), (-3, 2), (0, 5)] {
                let result = interp::run_function_within(
                    &c,
                    f,
                    vec![
                        Value::Int(APInt::new(32, lo as u32 as u64)),
                        Value::Int(APInt::new(32, hi as u32 as u64)),
                    ],
                    Some(200),
                );
                if fault && lo < hi {
                    assert!(matches!(result, Err(interp::InterpError::Undefined(_))));
                } else {
                    assert!(
                        matches!(&result.unwrap()[..], [Value::Int(v)] if *v == APInt::new(32, lo.max(hi) as u32 as u64))
                    );
                }
            }
            for args in [
                vec![Value::Poison, Value::Int(APInt::new(32, 5))],
                vec![Value::Int(APInt::new(32, 0)), Value::Poison],
            ] {
                let error = interp::run_function_within(&c, f, args, Some(200)).unwrap_err();
                if source == &conditional {
                    assert!(matches!(error, interp::InterpError::Undefined(_)));
                } else {
                    assert!(matches!(error, interp::InterpError::Message(_)));
                }
            }
        }
    }
}

#[test]
fn ordinary_layouts_follow_each_access_scope_and_reject_malformed_ancestry() {
    for (outer, inner, nested, keep, malformed) in [
        (None, None, false, false, false),
        (Some(LE64), None, false, false, false),
        (Some(BE64), None, false, true, false),
        (Some(PTR32), None, false, true, false),
        (
            Some("#layout.spec<endianness = \"little\">"),
            None,
            false,
            true,
            false,
        ),
        (Some(LE64), Some(BE64), true, true, false),
        (Some(LE64), Some(PTR32), true, true, false),
        (Some(BE64), Some(LE64), false, false, false),
        (Some("7"), None, false, true, true),
        (Some("7"), Some(LE64), false, true, true),
        (Some(LE64), Some("7"), true, true, true),
    ] {
        let header = outer.map_or_else(
            || "module".to_owned(),
            |spec| format!("module {{layout.spec = {spec}}}"),
        );
        let source = WRITE_ONLY.replace("module {layout.spec = #layout_spec}", &header);
        let (c, f, _, _) = fixture(&source);
        if let Some(spec) = inner {
            let module = tir::parse::ir::parse_operation(
                &c,
                &format!("module {{layout.spec = {spec}}} {{ module_end }}"),
                &MODULE,
            )
            .unwrap();
            let owner = if nested {
                c.get_region(c.get_op(f).regions()[0])
                    .iter(c.clone())
                    .next()
                    .unwrap()
                    .op_ids()
                    .into_iter()
                    .find(|&id| c.get_op(id).is_operation(&tir::scf::ORDERED_FOR))
                    .unwrap()
            } else {
                f
            };
            let mut attributes = c.get_op(owner).attributes();
            attributes.extend(module.attributes());
            c.set_op_attributes(owner, attributes);
        }
        let result = tir::passes::EqSatPass::new().run(f, &c, &AnalysisManager::new());
        if !malformed {
            result.unwrap();
        }
        let output = printed_memory(&c, f);
        assert_eq!(
            output.contains("ptr.alloca"),
            keep,
            "outer={outer:?} inner={inner:?}\n{output}"
        );
        assert_eq!(
            output.contains("ptr.store"),
            keep,
            "outer={outer:?} inner={inner:?}\n{output}"
        );
    }
}

#[test]
fn write_only_cells_keep_escapes_partial_accesses_and_observed_nested_writes() {
    for source in [
        WRITE_ONLY.replace(
            "ptr.store %5, %2",
            "ptr.store %5, %2 {unknown_contract = true}",
        ),
        WRITE_ONLY.replace("align = 4", "align = 4, unknown_contract = true"),
        WRITE_ONLY.replace("size = 4", "size = 1"),
        WRITE_ONLY.replace(
            "    func.return",
            "    %escape = ptr.cmp %2, %2 {predicate = \"eq\"} : !i1\n    func.return",
        ),
        WRITE_ONLY.replace(
            "    func.return",
            "    %read = ptr.load %2 : !i32\n    func.return",
        ),
    ] {
        let (c, f, _, _) = fixture(&source);
        tir::passes::EqSatPass::new()
            .run(f, &c, &AnalysisManager::new())
            .unwrap();
        let output = printed_memory(&c, f);
        assert!(output.contains("ptr.alloca"), "{output}");
        assert!(output.contains("ptr.store"), "{output}");
    }
}
