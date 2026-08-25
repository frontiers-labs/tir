//! Unit tests for the `tir-x86_64` backend's public API.

use tir_x86_64::{Feature, TargetConfig};

/// The one per-opcode record the backend describes `name` with.
fn info(name: &str) -> &'static tir::backend::InstrInfo {
    tir_x86_64::instruction_infos()
        .iter()
        .copied()
        .find(|info| info.name == name)
        .unwrap_or_else(|| panic!("x86-64 declares no instruction '{name}'"))
}

#[test]
fn instruction_info_carries_every_per_opcode_fact() {
    // One record per opcode, keyed by op name — `add32_norex` and `add` share a
    // mnemonic but not a record, so neither can reach the other's facts.
    let norex = info("add32_norex");
    assert_eq!(norex.mnemonic, "add");
    assert!(norex.asm.is_some());
    assert!(norex.encode.is_some());
    assert_eq!(norex.sched.len(), 1);
    assert_ne!(
        norex.sched[0],
        tir::backend::sched::InstrSchedClass::DEFAULT
    );
    assert_eq!(norex.effects, tir::backend::MemoryEffects::NONE);
}

#[test]
fn guarded_relaxations_hold_for_all_rules() {
    let context = tir::Context::with_default_dialects();
    let config = TargetConfig::parse("x86_64", None, None).unwrap();
    let rules = tir_x86_64::get_isel_rules(&context, config.features());
    tir::backend::isel::prove_guarded_relaxations(&rules).unwrap();
}

#[test]
fn x86_64_target_enables_required_features() {
    let config = TargetConfig::parse("x86_64", None, None).unwrap();
    assert_eq!(
        config.features(),
        &[Feature::X86, Feature::X86_64, Feature::SSE, Feature::SSE2,]
    );
    assert!(TargetConfig::parse("x86", None, None).is_err());
}

#[test]
fn generated_abi_matches_sysv_register_convention() {
    let target = tir::backend::select_target("x86_64", None, None).unwrap();
    let abi = target.abi();
    let int_args = abi
        .args
        .iter()
        .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Int)
        .unwrap();
    let int_rets = abi
        .rets
        .iter()
        .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Int)
        .unwrap();
    let float_args = abi
        .args
        .iter()
        .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Float)
        .unwrap();
    let float_rets = abi
        .rets
        .iter()
        .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Float)
        .unwrap();

    assert_eq!(abi.name, "sysv");
    assert_eq!(abi.sp, (int_args.regs[0].0, 4));
    assert_eq!(abi.ra, None);
    assert_eq!(abi.fp, Some((int_args.regs[0].0, 5)));
    assert_eq!(abi.stack.align, 16);
    assert_eq!(abi.stack.slot_size, 8);
    assert_eq!(abi.stack.save_style, tir::backend::abi::SaveStyle::PushPop);
    assert_eq!(
        int_args
            .regs
            .iter()
            .map(|register| register.1)
            .collect::<Vec<_>>(),
        vec![7, 6, 2, 1, 8, 9]
    );
    assert_eq!(
        int_rets
            .regs
            .iter()
            .map(|register| register.1)
            .collect::<Vec<_>>(),
        vec![0, 2]
    );
    assert_eq!(
        float_args
            .regs
            .iter()
            .map(|register| register.1)
            .collect::<Vec<_>>(),
        (0..=7).collect::<Vec<_>>()
    );
    assert_eq!(
        float_rets
            .regs
            .iter()
            .map(|register| register.1)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        abi.callee_saved
            .iter()
            .map(|register| register.1)
            .collect::<Vec<_>>(),
        vec![3, 5, 12, 13, 14, 15]
    );
}
