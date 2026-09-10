use tir::backend::isel::{
    prove_guarded_relaxations, EmitRequest, RegisterCapability, RegisterRequirement, Rule,
    RuleMatch, LATENCY_COST_SCALE,
};
use tir::sem::{SemGraph, SymKind};
use tir::{Context, Operation, PassError};

use super::fixtures::{binary, constant, nary, symbol};

fn no_emit(_: &Context, _: &EmitRequest, _: &RuleMatch) -> Result<Box<dyn Operation>, PassError> {
    unreachable!()
}

fn rule(kind: SymKind, rounded: SymKind, width: u32, replacement: u64, mode: u64) -> Rule {
    let arity = kind.arity();
    let mut candidate = SemGraph::new();
    let inputs: Vec<_> = (0..arity)
        .map(|i| symbol(&mut candidate, i as u32))
        .collect();
    nary(&mut candidate, kind, &inputs);

    let mut full = SemGraph::new();
    let mut inputs: Vec<_> = (0..arity).map(|i| symbol(&mut full, i as u32)).collect();
    inputs.push(constant(&mut full, mode, 3));
    let result = nary(&mut full, rounded, &inputs);
    let ordered = binary(&mut full, SymKind::Ge, result, result);
    let bits = constant(&mut full, replacement, width);
    let nan = nary(&mut full, SymKind::AsFloat, &[bits]);
    nary(&mut full, SymKind::If, &[ordered, result, nan]);
    let register = RegisterRequirement::whole(RegisterCapability::float(width));
    Rule {
        guarded_semantics: Some(full),
        operand_registers: (0..arity).map(|i| (i as u32, register)).collect(),
        result_register: Some(register),
        ..Rule::new("ieee-result", candidate, LATENCY_COST_SCALE, no_emit)
    }
}

#[test]
fn ieee_arithmetic_accepts_quiet_nan_payload_choice() {
    for (width, nan) in [(32, 0xffc01234), (64, 0x7ff8000000001234)] {
        for (kind, rounded) in [
            (SymKind::FAdd, SymKind::FAddRound),
            (SymKind::FSub, SymKind::FSubRound),
            (SymKind::FMul, SymKind::FMulRound),
            (SymKind::FDiv, SymKind::FDivRound),
            (SymKind::Sqrt, SymKind::SqrtRound),
            (SymKind::Fma, SymKind::FmaRound),
        ] {
            assert!(prove_guarded_relaxations(&[rule(kind, rounded, width, nan, 0)]).is_ok());
        }
    }
}

#[test]
fn ieee_arithmetic_rejects_finite_result_corruption() {
    use tir::graph::Dag;

    let mut rule = rule(SymKind::FAdd, SymKind::FAddRound, 32, 0x7fc00000, 0);
    let full = rule.guarded_semantics.as_mut().unwrap();
    let original = full.root().unwrap();
    let result = full.children(original).nth(1).unwrap();
    let ordered = binary(full, SymKind::Ge, result, result);
    let zero = constant(full, 0, 32);
    let zero_float = nary(full, SymKind::AsFloat, &[zero]);
    nary(full, SymKind::If, &[ordered, zero_float, original]);
    assert!(prove_guarded_relaxations(&[rule]).is_err());
}

#[test]
fn ieee_arithmetic_rejects_signaling_nan_replacement() {
    for (width, nan) in [(32, 0x7f800001), (64, 0x7ff0000000000001)] {
        let rule = rule(SymKind::FAdd, SymKind::FAddRound, width, nan, 0);
        assert!(prove_guarded_relaxations(&[rule]).is_err());
    }
}

#[test]
fn ieee_arithmetic_rejects_different_operands() {
    let mut rule = rule(SymKind::FSub, SymKind::FSubRound, 32, 0x7fc00000, 0);
    let mut candidate = SemGraph::new();
    let a = symbol(&mut candidate, 0);
    let b = symbol(&mut candidate, 1);
    binary(&mut candidate, SymKind::FSub, b, a);
    rule.pattern = candidate;
    assert!(prove_guarded_relaxations(&[rule]).is_err());
}

#[test]
fn ieee_arithmetic_rejects_different_operation() {
    let rule = rule(SymKind::FSub, SymKind::FAddRound, 32, 0x7fc00000, 0);
    assert!(prove_guarded_relaxations(&[rule]).is_err());
}

#[test]
fn ieee_arithmetic_rejects_different_rounding() {
    for mode in 1..=4 {
        let rule = rule(SymKind::FAdd, SymKind::FAddRound, 32, 0x7fc00000, mode);
        assert!(prove_guarded_relaxations(&[rule]).is_err());
    }
}

#[test]
fn float_copy_keeps_nan_payload_bits() {
    let mut candidate = SemGraph::new();
    symbol(&mut candidate, 0);
    let mut full = SemGraph::new();
    let value = symbol(&mut full, 0);
    let ordered = binary(&mut full, SymKind::Ge, value, value);
    let nan = constant(&mut full, 0x7fc00000, 32);
    let nan_float = nary(&mut full, SymKind::AsFloat, &[nan]);
    nary(&mut full, SymKind::If, &[ordered, value, nan_float]);
    let register = RegisterRequirement::whole(RegisterCapability::float(32));
    let rule = Rule {
        guarded_semantics: Some(full),
        operand_registers: vec![(0, register)],
        result_register: Some(register),
        ..Rule::new("float-copy", candidate, LATENCY_COST_SCALE, no_emit)
    };
    assert!(prove_guarded_relaxations(&[rule]).is_err());
}

#[test]
fn ieee_arithmetic_preserves_signed_zero() {
    use tir::graph::Dag;

    let mut rule = rule(SymKind::FAdd, SymKind::FAddRound, 32, 0x7fc00000, 0);
    let full = rule.guarded_semantics.as_mut().unwrap();
    let original = full.root().unwrap();
    let result = full.children(original).nth(1).unwrap();
    let zero = constant(full, 0, 32);
    let zero_float = nary(full, SymKind::AsFloat, &[zero]);
    let is_zero = binary(full, SymKind::Eq, result, zero_float);
    nary(full, SymKind::If, &[is_zero, zero_float, original]);
    assert!(prove_guarded_relaxations(&[rule]).is_err());
}
