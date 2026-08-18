//! Isel rule machinery: register capabilities, guarded relaxations and
//! table-driven rule construction.

use tir::backend::isel::{
    build_rules, prove_guarded_relaxations, CapabilityKind, EmitRequest, PatternRef,
    RegOperandSpec, RegisterCapability, RegisterRequirement, ResultRegSpec, Rule, RuleKind,
    RuleMatch, RuleSpec, LATENCY_COST_SCALE,
};
use tir::backend::regalloc::{RegClassId, RegClassInfo, RegisterView};
use tir::sem::{FloatFormat, SemBlobBuilder, SemGraph, SemOp, SemPayloadDesc, SemType, SymKind};
use tir::{Context, Operation, PassError};

use super::fixtures::{binary, constant, nary, r, symbol};

#[test]
fn overlapping_register_capability_accepts_integer_and_float_values() {
    let capability = RegisterCapability::any(64);

    assert!(capability.accepts(&SemType::bits(32)));
    assert!(capability.accepts(&SemType::bits(64)));
    assert!(capability.accepts(&SemType::Float(FloatFormat::new(11, 52))));
}

#[test]
fn whole_register_requirement_rejects_narrow_integer_values() {
    let requirement = RegisterRequirement::whole(RegisterCapability::integer(64));

    assert!(!requirement.accepts(&SemType::bits(32)));
    assert!(requirement.accepts(&SemType::bits(64)));
}

fn div_pattern() -> SemGraph {
    let mut g = SemGraph::new();
    let a = symbol(&mut g, 0);
    let b = symbol(&mut g, 1);
    binary(&mut g, SymKind::Div, a, b);
    g
}

fn emit_unreachable(
    _context: &Context,
    _req: &EmitRequest,
    _m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    unreachable!("the relaxation gate never emits")
}

/// `If(Eq(b, guard_rhs), ones, Div(lhs, rhs))` — the riscv-div shape, with the
/// guard constant and the else-arm operand order made configurable so the tests
/// can build both the sound rule and the two unsound variants.
fn guarded_div(guard_rhs: u64, else_swapped: bool) -> SemGraph {
    let mut g = SemGraph::new();
    let a = symbol(&mut g, 0);
    let b = symbol(&mut g, 1);
    let (num, den) = if else_swapped { (b, a) } else { (a, b) };
    let div = binary(&mut g, SymKind::Div, num, den);
    let guard_const = constant(&mut g, guard_rhs, 64);
    let cond = binary(&mut g, SymKind::Eq, b, guard_const);
    let ones = constant(&mut g, u64::MAX, 64);
    nary(&mut g, SymKind::If, &[cond, ones, div]);
    g
}

#[test]
fn guarded_div_rule_with_correct_relaxation_is_accepted() {
    let rule = Rule::new("div", div_pattern(), LATENCY_COST_SCALE, emit_unreachable)
        .with_guarded_semantics(guarded_div(0, false));
    assert!(prove_guarded_relaxations(&[rule]).is_ok());
}

#[test]
fn guarded_div_rule_with_wrong_guard_region_is_rejected() {
    // Guarding on `b == 1` instead of `b == 0` leaves the pure `div` unequal to
    // the behavior at `b == 1` (where `div(a,1) == a`, not all-ones).
    let rule = Rule::new("div", div_pattern(), LATENCY_COST_SCALE, emit_unreachable)
        .with_guarded_semantics(guarded_div(1, false));
    match prove_guarded_relaxations(&[rule]) {
        Err(PassError::InvalidRuleSet(msg)) => assert!(msg.contains("div")),
        Err(other) => panic!("expected InvalidRuleSet, got {other:?}"),
        Ok(_) => panic!("expected InvalidRuleSet, rule was accepted"),
    }
}

#[test]
fn guarded_div_rule_with_mismatched_else_arm_is_rejected() {
    // The else arm computes `div(b, a)` while the selection pattern is `div(a, b)`.
    let rule = Rule::new("div", div_pattern(), LATENCY_COST_SCALE, emit_unreachable)
        .with_guarded_semantics(guarded_div(0, true));
    match prove_guarded_relaxations(&[rule]) {
        Err(PassError::InvalidRuleSet(msg)) => assert!(msg.contains("div")),
        Err(other) => panic!("expected InvalidRuleSet, got {other:?}"),
        Ok(_) => panic!("expected InvalidRuleSet, rule was accepted"),
    }
}

fn symbol_blob() -> (Vec<u8>, Vec<SymKind>, u32) {
    let mut builder = SemBlobBuilder::new();
    let offset = builder.intern(&[
        SemOp::Node(SymKind::Symbol),
        SemOp::Payload(SemPayloadDesc::SymbolId(0)),
    ]);
    let (blob, kinds) = builder.finish();
    (blob, kinds, offset)
}

fn nop_emit(
    _context: &Context,
    _req: &EmitRequest,
    _m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    unreachable!()
}

fn rule_spec(offset: u32, features: &'static [u16]) -> RuleSpec {
    RuleSpec {
        name: "inst",
        features,
        pattern: PatternRef {
            offset,
            typed: false,
            float_width: None,
        },
        cost_terms: &[("add", 4)],
        kind: RuleKind::Value,
        prelude_emit: None,
        emit_fn: nop_emit,
        constraints: &[],
        registers: &[],
        result: None,
        imm_ranges: &[],
        guarded: None,
    }
}

#[test]
fn build_rules_gates_on_any_feature() {
    let context = Context::with_default_dialects();
    let (blob, kinds, offset) = symbol_blob();
    static BOTH: &[u16] = &[1, 2];
    let gated = rule_spec(offset, BOTH);
    let open = rule_spec(offset, &[]);
    let cost: fn(&str) -> u32 = |_| 3u32;
    let specs: &[&RuleSpec] = &[&gated, &open];

    let rules = build_rules(&context, &[2], &kinds, &blob, &[], cost, specs);
    assert_eq!(rules.len(), 2);
    let rules = build_rules(&context, &[9], &kinds, &blob, &[], cost, specs);
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].base_cost, 3 * LATENCY_COST_SCALE + 4);
}

#[test]
fn build_rules_resolves_register_widths() {
    let context = Context::with_default_dialects();
    let (blob, kinds, offset) = symbol_blob();
    static WIDE: RegClassInfo = RegClassInfo {
        name: "W",
        file: "W",
        registers: &[0],
        group_width: 1,
        view: RegisterView {
            bit_offset: 8,
            merge: true,
        },
    };
    static REGS: &[RegOperandSpec] = &[RegOperandSpec {
        symbol: 0,
        class: r(),
        whole: true,
        capability: CapabilityKind::Integer,
    }];
    let mut spec = rule_spec(offset, &[]);
    spec.registers = REGS;
    spec.result = Some(ResultRegSpec {
        class: RegClassId::new(&WIDE),
        capability: CapabilityKind::Any,
    });
    let rules = build_rules(&context, &[], &kinds, &blob, &[("R", 32)], |_| 0, &[&spec]);
    assert_eq!(rules.len(), 1);
    // `W` has no width under the enabled features: the result register
    // requirement drops out, the known-class operand keeps its width.
    assert_eq!(rules[0].operand_registers.len(), 1);
    assert!(rules[0].result_register.is_none());
    assert_eq!(rules[0].operand_registers[0].0, 0);
}
