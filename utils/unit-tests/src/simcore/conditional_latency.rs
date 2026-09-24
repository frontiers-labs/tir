#![allow(dead_code, unused_imports)]

use tir::backend::{AsmDialect, MachineContext};
use tir::helpers::{dialect, operation};
use tir_sim::{Executor, ProgramImage};

include!(concat!(env!("OUT_DIR"), "/latency.rs"));
dialect! {
    LatencyDialect {
        name: "latency",
        operation_file: concat!(env!("OUT_DIR"), "/latency_ops.rs"),
        type_parsers: reg_class_type_parsers(),
    }
}

struct FusionEvents(Vec<(usize, usize, &'static str)>);

impl tir_sim::scoreboard::EventHandler for FusionEvents {
    fn fused_group(
        &mut self,
        first: usize,
        members: usize,
        name: &'static str,
        _decoded_uops: u16,
        _execution_uops: u16,
    ) {
        self.0.push((first, members, name));
    }

    fn render(&self) -> String {
        String::new()
    }
}

fn fusion_pair(
    src_index: u16,
    second_imm: i64,
    first_pc: u64,
    layout_known: bool,
) -> (
    tir::Context,
    [tir_sim::scoreboard::ScoreboardInstr; 2],
    tir_sim::scoreboard::Prf,
) {
    use tir::attributes::AttributeValue;
    use tir::backend::liveness::execution_regs;
    use tir::backend::{phys_attr, MachineInstruction};
    use tir::Operation;
    use tir_sim::scoreboard::{phys_regs, Prf, ScoreboardInstr};

    let context = tir::Context::with_default_dialects();
    context.register_dialect::<LatencyDialect>();
    let model = fusion_core_model();
    let prf = Prf {
        class_to_file: std::collections::HashMap::from([("Gpr".to_string(), "Gpr".to_string())]),
        capacity: std::collections::HashMap::new(),
    };
    let widths = [("Gpr", 64)];
    let specifications = [(0, 1, 1), (1, src_index, second_imm)];
    let mut slots = Vec::new();
    let mut pc = first_pc;
    for (index, src, imm) in specifications {
        let op_id = AddImmOpBuilder::new(&context)
            .attr("dst", phys_attr((RegClass::Gpr.id(), 1)))
            .attr("src", phys_attr((RegClass::Gpr.id(), src)))
            .attr("imm", AttributeValue::Int(imm))
            .build();
        let op = context.get_op(op_id.id());
        let mi = op.clone().as_interface::<dyn MachineInstruction>().unwrap();
        let info = mi.info();
        let regs = execution_regs(&op);
        let width_bytes = u16::from(mi.width_bytes());
        slots.push(ScoreboardInstr {
            text: format!("addi r1, r{src}, {imm}"),
            op_name: info.name.to_string(),
            class: info.sched_on(&model),
            defs: phys_regs(&regs.phys_defs, Some(&prf)),
            uses: phys_regs(&regs.phys_uses, Some(&prf)),
            or_updates: Vec::new(),
            fusion_operands: tir_sim::timing::fusion_operands(&op, info, Some(&prf), &widths),
            fusion_boundary: index == 0,
            layout_known,
            encoded_bytes: None,
            branch: None,
            pc,
            width_bytes,
            mem: Vec::new(),
        });
        pc += u64::from(width_bytes);
    }
    (context, [slots.remove(0), slots.remove(0)], prf)
}

fn fusion_groups(
    base: &[tir_sim::scoreboard::ScoreboardInstr],
    prf: &tir_sim::scoreboard::Prf,
) -> Vec<(usize, usize, &'static str)> {
    let model = fusion_core_model();
    let mut events = FusionEvents(Vec::new());
    tir_sim::scoreboard::run(
        &model,
        base,
        1,
        &tir_sim::scoreboard::TimingConfig::for_model(&model),
        None,
        Some(prf),
        None,
        Some(&mut events),
    );
    events.0
}

fn record() -> (tir::Context, Executor) {
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<AsmDialect>();
    context.register_dialect::<LatencyDialect>();
    let parser = tir::backend::AsmParser::new(get_instruction_parsers(Feature::ALL).0);
    let module = parser
        .parse_asm(
            &context,
            ".global first\nfirst:\ndiv r1, r2, r1\ndiv r1, r2, r1\ndiv r1, r2, r1\ndiv r1, r2, r1",
        )
        .unwrap();
    let program = ProgramImage::from_module(&context, module, 0, Some("first")).unwrap();
    let mut executor = Executor::new(4096);
    executor.enable_trace_recording();
    executor.set_timing_model(test_core_model());
    executor
        .write_register("Gpr", 1, tir::utils::APInt::new(64, 1))
        .unwrap();
    executor
        .write_register("Gpr", 2, tir::utils::APInt::new(64, 8))
        .unwrap();
    executor.load(program).unwrap();
    executor.run(6, 10).unwrap();
    (context, executor)
}

#[test]
fn conditional_schedule_trace_captures_each_entry_class() {
    let (_, executor) = record();
    let trace = executor.sched_trace();
    assert_eq!(
        trace.iter().map(|class| class.latency).collect::<Vec<_>>(),
        [4, 20, 4]
    );
    assert_eq!(trace[0].uops[0].routes[0].resources[1].cycles, 5);
    assert_eq!(trace[1].uops[0].routes[0].resources[1].cycles, 20);
}

#[test]
fn conditional_latency_replay_uses_captured_values() {
    use tir_sim::predictor::AlwaysNotTaken;
    use tir_sim::timing::{simulate, TimingConfig};
    let (context, executor) = record();
    let model = test_core_model();
    let config = TimingConfig::for_model(&model);
    let timing = |classes| {
        simulate(
            &model,
            &context,
            executor.trace(),
            classes,
            &config,
            &mut AlwaysNotTaken,
            None,
            &[],
            None,
            None,
            None,
            None,
        )
    };
    assert_eq!(
        timing(None).cycles - timing(Some(executor.sched_trace())).cycles,
        31
    );
    let mut fallback_occupancy = executor.sched_trace().to_vec();
    let fallback_uops = fallback_occupancy[1].uops;
    fallback_occupancy[0].uops = fallback_uops;
    fallback_occupancy[2].uops = fallback_uops;
    assert_eq!(
        timing(Some(&fallback_occupancy)).cycles - timing(Some(executor.sched_trace())).cycles,
        15
    );
}

#[test]
fn conditional_latency_fallback_does_not_require_a_schedule_block() {
    use tir::attributes::AttributeValue;
    use tir::backend::MachineInstruction;
    use tir::Operation;
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<LatencyDialect>();
    let op = AddImmOpBuilder::new(&context)
        .attr("imm", AttributeValue::Int(7))
        .build();
    let mi = context
        .get_op(op.id())
        .as_interface::<dyn MachineInstruction>()
        .unwrap();
    assert_eq!(
        mi.sched_on(&immediate_core_model(), &mut Executor::new(0))
            .latency,
        9
    );
}

fn div_latency(model: &tir::backend::sched::MachineModel, rhs: Option<u16>, value: u64) -> u16 {
    use tir::backend::{phys_attr, MachineInstruction};
    use tir::Operation;
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<LatencyDialect>();
    let mut builder = DivOpBuilder::new(&context);
    if let Some(rhs) = rhs {
        builder = builder.attr("rhs", phys_attr((RegClass::Gpr.id(), rhs)));
    }
    let op = builder.build();
    let mi = context
        .get_op(op.id())
        .as_interface::<dyn MachineInstruction>()
        .unwrap();
    let mut executor = Executor::new(0);
    executor
        .write_register("Gpr", rhs.unwrap_or(1), tir::utils::APInt::new(64, value))
        .unwrap();
    mi.sched_on(model, &mut executor).latency
}

fn div_schedule(
    model: &tir::backend::sched::MachineModel,
    value: u64,
) -> tir::backend::sched::InstrSchedClass {
    use tir::backend::{phys_attr, MachineInstruction};
    use tir::Operation;
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<LatencyDialect>();
    let op = DivOpBuilder::new(&context)
        .attr("rhs", phys_attr((RegClass::Gpr.id(), 1)))
        .build();
    let mi = context
        .get_op(op.id())
        .as_interface::<dyn MachineInstruction>()
        .unwrap();
    let mut executor = Executor::new(0);
    executor
        .write_register("Gpr", 1, tir::utils::APInt::new(64, value))
        .unwrap();
    mi.sched_on(model, &mut executor)
}

#[test]
fn conditional_latency_without_uops_inherits_fallback_resources() {
    let class = div_schedule(&test_core_model(), 2);
    assert_eq!(class.latency, 6);
    assert_eq!(class.uops[0].routes[0].resources[1].cycles, 20);
}

#[test]
fn conditional_latency_uses_first_matching_case() {
    assert_eq!(div_latency(&ordered_core_model(), Some(1), 1), 4);
}

#[test]
fn conditional_latency_stops_at_an_unknown_case() {
    assert_eq!(div_latency(&ordered_core_model(), None, 1), 20);
}

#[test]
fn conditional_latency_falls_back_when_no_case_matches() {
    assert_eq!(div_latency(&test_core_model(), Some(1), 8), 20);
}

#[test]
fn conditional_latency_distinguishes_register_number_from_contents() {
    assert_eq!(div_latency(&register_core_model(), Some(1), 8), 3);
    assert_eq!(div_latency(&register_core_model(), Some(2), 1), 20);
}

#[test]
fn conditional_latency_immediate_needs_no_register_values() {
    use tir::attributes::AttributeValue;
    use tir::backend::MachineInstruction;
    use tir::Operation;
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<LatencyDialect>();
    let op = AddImmOpBuilder::new(&context)
        .attr("imm", AttributeValue::Int(0))
        .build();
    let mi = context
        .get_op(op.id())
        .as_interface::<dyn MachineInstruction>()
        .unwrap();
    assert_eq!(
        mi.sched_on(&immediate_core_model(), &mut Executor::new(0))
            .latency,
        2
    );
}

#[test]
fn conditional_latency_uses_later_matching_case() {
    assert_eq!(div_latency(&ordered_core_model(), Some(1), 8), 7);
}

#[test]
fn conditional_latency_empty_machine_cases_use_static_class() {
    assert_eq!(div_latency(&immediate_core_model(), Some(1), 1), 20);
}

#[test]
fn generated_fusion_rule_checks_register_immediate_and_layout_facts() {
    for (src, imm, pc, layout_known) in [
        (1, 2, 0, true),  // matching register, immediates, and block
        (2, 2, 0, true),  // wrong source register
        (1, 3, 0, true),  // wrong immediate
        (1, 2, 31, true), // pair crosses a 32-byte block
        (1, 2, 0, false), // no encoded layout was available
    ] {
        let (_context, pair, prf) = fusion_pair(src, imm, pc, layout_known);
        let groups = fusion_groups(&pair, &prf);
        if src == 1 && imm == 2 && pc == 0 && layout_known {
            assert_eq!(groups, [(0, 2, "AddImmediatePair")]);
        } else {
            assert!(
                groups.is_empty(),
                "unexpected fusion for {src}, {imm}, {pc}, {layout_known}"
            );
        }
    }
}

#[test]
fn conditional_latency_invalid_extract_uses_fallback() {
    assert_eq!(div_latency(&invalid_extract_core_model(), Some(1), 1), 20);
}

#[test]
fn conditional_latency_invalid_extension_uses_fallback() {
    assert_eq!(div_latency(&invalid_extension_core_model(), Some(1), 1), 20);
}
