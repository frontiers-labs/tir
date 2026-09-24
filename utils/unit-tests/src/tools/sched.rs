use tir::backend::sched::{InstrSchedClass, MachineModel};
use tir_sim::scoreboard::{ScoreboardInstr, SimContext};
use tir_tools::sched::event::{make, View};

const MODEL: MachineModel = MachineModel {
    name: "test",
    id: usize::MAX,
    issue_width: 1,
    frontend: None,
    resources: &[],
    buffers: &[],
    pipeline: &[],
    forwards: &[],
    reg_files: &[],
    fusions: &[],
};

fn instruction(text: &str) -> ScoreboardInstr {
    ScoreboardInstr {
        text: text.to_string(),
        op_name: text.to_string(),
        class: InstrSchedClass::DEFAULT,
        defs: Vec::new(),
        uses: Vec::new(),
        or_updates: Vec::new(),
        branch: None,
        pc: 0,
        width_bytes: 1,
        mem: Vec::new(),
        fusion_operands: Vec::new(),
        layout_known: false,
        fusion_boundary: false,
        encoded_bytes: None,
    }
}

#[test]
fn fusion_report_counts_only_selected_groups_across_iterations() {
    let base = [instruction("first"), instruction("second")];
    let context = SimContext {
        model: &MODEL,
        iterations: 3,
        base: &base,
    };
    let mut handler = make(View::Resource);
    handler.start(&context);
    handler.fused_group(0, 2, "pair", 1, 1);
    handler.fused_group(4, 2, "pair", 1, 1);

    let report = handler.render();
    assert!(report.contains("Decoded uops:      4"));
    assert!(report.contains("Execution uops:    4"));
    assert!(report.contains("Fused groups:      2"));
}

#[test]
fn zero_idiom_class_keeps_execution_cost_when_operands_differ() {
    let mut xor = instruction("xor r1, r2");
    xor.class = InstrSchedClass {
        zero_idiom: true,
        ..InstrSchedClass::DEFAULT
    };
    xor.uses.push(("GPR".to_string(), 2));
    xor.defs.push(("GPR".to_string(), 1));
    let base = [xor];
    let context = SimContext {
        model: &MODEL,
        iterations: 1,
        base: &base,
    };
    let mut handler = make(View::Resource);
    handler.start(&context);

    assert!(handler.render().contains("Execution uops:    1"));
}
