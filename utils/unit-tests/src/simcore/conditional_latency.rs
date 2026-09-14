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
fn conditional_latency_trace_captures_each_entry_value() {
    let (_, executor) = record();
    assert_eq!(executor.latency_trace(), &[4, 20, 4]);
}

#[test]
fn conditional_latency_replay_uses_captured_values() {
    use tir_sim::predictor::AlwaysNotTaken;
    use tir_sim::timing::{simulate, TimingConfig};
    let (context, executor) = record();
    let model = test_core_model();
    let config = TimingConfig::for_model(&model);
    let timing = |latencies| {
        simulate(
            &model,
            &context,
            executor.trace(),
            latencies,
            &config,
            &mut AlwaysNotTaken,
            None,
            None,
            None,
            None,
        )
    };
    assert_eq!(
        timing(None).cycles - timing(Some(executor.latency_trace())).cycles,
        32
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
fn conditional_latency_invalid_extract_uses_fallback() {
    assert_eq!(div_latency(&invalid_extract_core_model(), Some(1), 1), 20);
}

#[test]
fn conditional_latency_invalid_extension_uses_fallback() {
    assert_eq!(div_latency(&invalid_extension_core_model(), Some(1), 1), 20);
}
