//! Unit tests for the `tir-arm64` backend's public API.

use tir::Context;
use tir_arm64::{Feature, RegClass, TargetConfig};

/// The one per-opcode record the backend describes `name` with.
fn info(name: &str) -> &'static tir::backend::InstrInfo {
    tir_arm64::instruction_infos()
        .iter()
        .copied()
        .find(|info| info.name == name)
        .unwrap_or_else(|| panic!("arm64 declares no instruction '{name}'"))
}

#[test]
fn instruction_info_carries_every_per_opcode_fact() {
    // One record per opcode: `add` prints, encodes and schedules through the
    // fields of its own `InstrInfo`, with no side table keyed by its name.
    let add = info("add");
    assert_eq!(add.mnemonic, "add");
    assert_eq!(add.width_bytes, (4, 4));
    assert!(add.asm.is_some());
    assert!(add.encode.is_some());
    assert_eq!(add.sched.len(), tir_arm64::machines(Feature::ALL).len());
    assert_eq!(add.effects, tir::backend::MemoryEffects::NONE);
}

#[test]
fn guarded_relaxations_hold_for_all_rules() {
    let context = Context::with_default_dialects();
    let rules = tir_arm64::get_isel_rules(&context, Feature::ALL);
    tir::backend::isel::prove_guarded_relaxations(&rules).unwrap();
}

#[test]
fn generated_abi_matches_aapcs64_register_convention() {
    let abi = tir_arm64::default_abi();
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
    let vector_rets = abi
        .rets
        .iter()
        .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Vector)
        .unwrap();

    assert_eq!(abi.name, "aapcs64");
    assert_eq!(abi.sp, (RegClass::GPRsp.id(), 31));
    assert_eq!(abi.ra, Some((RegClass::GPR.id(), 30)));
    assert_eq!(abi.fp, Some((RegClass::GPR.id(), 29)));
    assert_eq!(abi.stack.align, 16);
    assert_eq!(abi.stack.slot_size, 8);
    assert_eq!(
        abi.stack.save_style,
        tir::backend::abi::SaveStyle::FrameSlots
    );
    assert_eq!(
        int_args
            .regs
            .iter()
            .map(|register| register.1)
            .collect::<Vec<_>>(),
        (0..=7).collect::<Vec<_>>()
    );
    assert_eq!(
        int_rets
            .regs
            .iter()
            .map(|register| register.1)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(float_args.regs[0], (RegClass::FPR64.id(), 0));
    assert_eq!(float_args.regs.last(), Some(&(RegClass::FPR64.id(), 7)));
    assert_eq!(
        vector_rets.regs,
        &[
            (RegClass::VPR.id(), 0),
            (RegClass::VPR.id(), 1),
            (RegClass::VPR.id(), 2),
            (RegClass::VPR.id(), 3),
        ]
    );
    assert_eq!(
        &abi.callee_saved[..11],
        &(19..=29)
            .map(|index| (RegClass::GPR.id(), index))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        &abi.callee_saved[11..],
        &(8..=15)
            .map(|index| (RegClass::VPR.id(), index))
            .collect::<Vec<_>>()
    );
}

#[test]
fn decoder_round_trips_golden_words() {
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);

    // The encoders are golden-verified against llvm-mc (checks/obj and
    // checks/asm), so `decode(word) -> op` is correct iff re-encoding that op
    // through the binary writer reproduces the original word. This exercises
    // operand extraction (registers, immediates, split fields) and
    // fixed-opcode matching across the instruction classes the benchmark ELFs
    // execute, plus `svc`.
    let cases: &[(u32, &str)] = &[
        (0x8B020020, "add x0, x1, x2"),
        (0x9AC22020, "lslv x0, x1, x2"),
        (0xEB02003F, "cmp x1, x2"),
        (0xF9400020, "ldr x0, [x1]"),
        (0xF9000062, "str x2, [x3]"),
        (0x54000080, "b.eq +16"),
        (0x14000003, "b +12"),
        (0x94000002, "bl +8"),
        (0xD65F03C0, "ret"),
        (0xD2800540, "movz x0, #42"),
        (0xD4000001, "svc #0"),
        (0xF1000400, "subs x0, x0, #1"),
        (0xB100094A, "adds x10, x10, #2"),
        (0xF8616801, "ldr x1, [x0, x1]"),
        (0xF860790D, "ldr x13, [x8, x0, lsl #3]"),
        (0xF82D696E, "str x14, [x11, x13]"),
        (0xF82B790D, "str x13, [x8, x11, lsl #3]"),
        (0xF81F0FFE, "str x30, [sp, #-16]!"),
        (0xF84107FE, "ldr x30, [sp], #16"),
        (0xF802050A, "str x10, [x8], #32"),
        (0xF8408C20, "ldr x0, [x1, #8]!"),
        (0xA9BF7BFD, "stp x29, x30, [sp, #-16]!"),
        (0xA8C17BFD, "ldp x29, x30, [sp], #16"),
        (0xD503201F, "nop"),
        (0xF2BBD5A9, "movk x9, #0xdead, lsl #16"),
        (0xF2C24689, "movk x9, #0x1234, lsl #32"),
        (0xF2F4B4A9, "movk x9, #0xa5a5, lsl #48"),
        (0xCA493129, "eor x9, x9, x9, lsr #12"),
        (0xCA096529, "eor x9, x9, x9, lsl #25"),
        (0xD37AE5AD, "lsl x13, x13, #6"),
        (0x52807D02, "movz w2, #1000"),
        (0x12001C00, "and w0, w0, #0xff"),
        (0x92401C41, "and x1, x2, #0xff"),
        (0x92402C83, "and x3, x4, #0xfff"),
        (0x1E612802, "fadd d2, d0, d1"),
        (0x1E600843, "fmul d3, d2, d0"),
        (0x1E601000, "fmov d0, #2.0"),
        (0x9E660064, "fmov x4, d3"),
        (0x9E670064, "fmov d4, x3"),
        (0x4E080C00, "dup v0.2d, x0"),
        (0x4EE18402, "add v2.2d, v0.2d, v1.2d"),
        (0x4EA18403, "add v3.4s, v0.4s, v1.4s"),
        (0x4E61D404, "fadd v4.2d, v0.2d, v1.2d"),
        (0x6E61DC05, "fmul v5.2d, v0.2d, v1.2d"),
        (0x3DC00008, "ldr q8, [x0]"),
        (0x3D800009, "str q9, [x0]"),
        (0x4F000426, "movi v6.4s, #1"),
        (0x6F00F407, "fmov v7.2d, #2.0"),
    ];

    let module = tir::builtin::ops::module(&context, None).build();
    let region = context.create_region();
    let block = context.create_block(vec![]);
    region.add_block(block.id());
    let bb = context.get_block(block.id());
    for &(w, asm) in cases {
        let id = tir_arm64::decode_instruction(&context, w)
            .unwrap_or_else(|| panic!("failed to decode {asm} ({w:#010x})"));
        bb.append(id);
    }
    bb.append_op(tir::backend::SymbolEndOpBuilder::new(&context).build());
    let symbol = tir::backend::SymbolOpBuilder::new(&context)
        .body(region.id())
        .attr(
            "name",
            tir::attributes::AttributeValue::Str("decoded".to_string().into()),
        )
        .build();
    module.body().append_op(symbol);
    module
        .body()
        .append_op(tir::builtin::ops::module_end(&context).build());

    let writer = tir::backend::binary::BinaryWriter::new();
    let format = target.object_format().unwrap();
    let obj = writer.write_module(&context, &module, &format).unwrap();
    let text = obj
        .sections
        .iter()
        .find(|section| section.name == ".text")
        .expect("re-encoded object must have .text");
    assert_eq!(text.data.len(), cases.len() * 4);
    for (i, &(w, asm)) in cases.iter().enumerate() {
        let bytes = &text.data[i * 4..i * 4 + 4];
        let word = u32::from_le_bytes(bytes.try_into().unwrap());
        assert_eq!(word, w, "round-trip mismatch for {asm}");
    }
}

fn features(march: &str) -> Vec<Feature> {
    TargetConfig::parse(march, None, None)
        .expect("march should parse")
        .features()
        .to_vec()
}

#[test]
fn generic_cpu_names_resolve_machine_models() {
    let target = tir::backend::select_target("arm64", Some("generic-ooo"), None).unwrap();
    assert_eq!(target.default_machine(), Some("arm64-ooo"));
    let target = tir::backend::select_target("arm64", Some("arm64-in-order"), None).unwrap();
    assert_eq!(target.default_machine(), Some("arm64-in-order"));
}

#[test]
fn march_enables_the_base_profile() {
    let config = TargetConfig::parse("arm64", None, None).unwrap();
    assert_eq!(
        config.features(),
        &[Feature::ARMv8A64, Feature::FP, Feature::AdvSIMD]
    );
    assert!(TargetConfig::parse("arm64", None, Some("-armv8a64")).is_err());
}

#[test]
fn march_selects_cumulative_architecture_revisions() {
    assert_eq!(
        features("armv8.0-a"),
        vec![Feature::ARMv8A64, Feature::FP, Feature::AdvSIMD]
    );
    assert_eq!(
        features("armv8.2-a"),
        vec![
            Feature::ARMv8A64,
            Feature::ARMv8_1A64,
            Feature::ARMv8_2A64,
            Feature::FP,
            Feature::AdvSIMD,
            Feature::LSE,
        ]
    );
    assert_eq!(
        features("armv9-a"),
        vec![
            Feature::ARMv8A64,
            Feature::ARMv8_1A64,
            Feature::ARMv8_2A64,
            Feature::ARMv8_3A64,
            Feature::ARMv8_4A64,
            Feature::ARMv8_5A64,
            Feature::FP,
            Feature::AdvSIMD,
            Feature::LSE,
            Feature::ARMv9A64,
        ]
    );
    assert_eq!(
        features("armv9.4-a"),
        vec![
            Feature::ARMv8A64,
            Feature::ARMv8_1A64,
            Feature::ARMv8_2A64,
            Feature::ARMv8_3A64,
            Feature::ARMv8_4A64,
            Feature::ARMv8_5A64,
            Feature::ARMv8_6A64,
            Feature::ARMv8_7A64,
            Feature::ARMv8_8A64,
            Feature::ARMv8_9A64,
            Feature::FP,
            Feature::AdvSIMD,
            Feature::LSE,
            Feature::ARMv9A64,
            Feature::ARMv9_1A64,
            Feature::ARMv9_2A64,
            Feature::ARMv9_3A64,
            Feature::ARMv9_4A64,
        ]
    );
}
