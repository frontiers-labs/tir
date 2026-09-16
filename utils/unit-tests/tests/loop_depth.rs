use std::process::Command;

use tir::backend::regalloc::RegisterAllocationPass;
use tir::backend::{fresh_reg, RegSlot};
use tir::{AnalysisManager, Context, Operation, OperationRef, Pass};
use tir_x86_64 as _;

fn spill_costs(source: &str) -> Vec<u64> {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "dump_fixture"])
        .env("TIR_LOOP_DEPTH_SOURCE", source)
        .env("TIR_PBQP_DUMP_DIR", directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let dumps: Vec<_> = std::fs::read_dir(directory.path()).unwrap().collect();
    assert_eq!(dumps.len(), 1);
    let task: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dumps[0].as_ref().unwrap().path()).unwrap()).unwrap();
    let mut costs: Vec<_> = task["node_costs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_array().unwrap().last().unwrap().as_u64().unwrap())
        .collect();
    costs.sort_unstable();
    costs
}

#[test]
#[ignore = "subprocess fixture for isolated PBQP dumping"]
fn dump_fixture() {
    let source = std::env::var("TIR_LOOP_DEPTH_SOURCE").unwrap();
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("x86_64", None, None).unwrap();
    target.register_dialects(&context);
    let symbol = tir::parse::ir::parse_ir::<tir::backend::SymbolOp>(&context, &source).unwrap();
    let body = context.get_op(symbol.id()).regions()[0];
    let hooks = target.regalloc_target();
    let class = hooks.register_info().class("GPR").unwrap();
    for block in context.get_region(body).iter(context.clone()) {
        let value = fresh_reg(&context, class);
        let define = hooks.emit_copy(
            &context,
            class,
            RegSlot::Value(value),
            RegSlot::Phys((class, 0)),
        );
        let use_value = hooks.emit_copy(
            &context,
            class,
            RegSlot::Phys((class, 1)),
            RegSlot::Value(value),
        );
        block.insert(0, define.id());
        block.insert(1, use_value.id());
    }
    RegisterAllocationPass::with_abi(hooks, target.abi())
        .run(
            &OperationRef::new(context.get_op(symbol.id())),
            &context,
            &AnalysisManager::new(),
        )
        .unwrap();
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 16,
        failure_persistence: None,
        ..proptest::test_runner::Config::default()
    })]

    #[test]
    fn multiple_latches_share_one_loop_depth(latches in 2usize..10, reverse in proptest::bool::ANY) {
        let mut source = String::from(
            "asm.symbol {name = \"multi\"} { %c = constant {value = 1} : !i1 cfg.br ^h0 asm.symbol_end "
        );
        let mut blocks = Vec::new();
        for i in 0..latches - 1 {
            let next = if i + 2 == latches { format!("l{}", i + 1) } else { format!("h{}", i + 1) };
            blocks.push(format!("^h{i}: cfg.cond_br %c, ^l{i}, ^{next} "));
        }
        for i in 0..latches {
            blocks.push(format!("^l{i}: cfg.br ^h0 "));
        }
        if reverse { blocks.reverse(); }
        source.extend(blocks);
        source.push('}');
        let mut costs = spill_costs(&source);
        // The condition is defined once outside and read once per decision.
        let condition = costs.iter().position(|&cost| cost == 10 + 100 * (latches as u64 - 1));
        proptest::prop_assert!(condition.is_some(), "costs: {:?}", costs);
        costs.remove(condition.unwrap());
        proptest::prop_assert_eq!(costs.len(), 2 * latches);
        let entry_cost = costs[0];
        proptest::prop_assert!(costs[1..].iter().all(|&cost| cost == 10 * entry_cost), "costs: {:?}", costs);
    }

    #[test]
    fn self_loop_does_not_include_its_preheader(preheaders in 1usize..10) {
        let mut source = String::from("asm.symbol {name = \"self_loop\"} { cfg.br ^p0 asm.symbol_end ");
        for i in 0..preheaders {
            let next = if i + 1 == preheaders { i } else { i + 1 };
            source.push_str(&format!("^p{i}: cfg.br ^p{next} "));
        }
        source.push('}');
        let costs = spill_costs(&source);
        proptest::prop_assert_eq!(costs.len(), preheaders + 1);
        proptest::prop_assert!(costs[..preheaders].iter().all(|&cost| cost == costs[0]));
        proptest::prop_assert_eq!(costs[preheaders], 10 * costs[0]);
    }
}
