use tir_symbolic::lang::SymKind;
use tir_symbolic::sem::{
    con, confirm_bool_via_if, confirm_extension_via_shifts, op, sym, EquivalenceOracle, FuzzOracle,
    SemGraph, SmtOracle,
};

#[test]
fn sign_extension_is_a_left_then_arithmetic_right_shift() {
    assert!(confirm_extension_via_shifts(
        SymKind::SExt,
        SymKind::ShiftRightArithmetic,
        &FuzzOracle::default(),
    ));
}

#[test]
fn zero_extension_is_a_left_then_logical_right_shift() {
    assert!(confirm_extension_via_shifts(
        SymKind::ZExt,
        SymKind::ShiftRightLogic,
        &FuzzOracle::default(),
    ));
}

#[test]
fn sign_extension_is_not_a_logical_right_shift() {
    // The oracle must reject the wrong pairing (srl can't sign-extend).
    assert!(!confirm_extension_via_shifts(
        SymKind::SExt,
        SymKind::ShiftRightLogic,
        &FuzzOracle::default(),
    ));
}

#[test]
fn smt_oracle_proves_extension_identities() {
    assert!(confirm_extension_via_shifts(
        SymKind::SExt,
        SymKind::ShiftRightArithmetic,
        &SmtOracle,
    ));
    assert!(confirm_extension_via_shifts(
        SymKind::ZExt,
        SymKind::ShiftRightLogic,
        &SmtOracle,
    ));
}

#[test]
fn smt_oracle_refutes_wrong_pairing() {
    assert!(!confirm_extension_via_shifts(
        SymKind::SExt,
        SymKind::ShiftRightLogic,
        &SmtOracle,
    ));
}

#[test]
fn smt_oracle_shares_symbols_across_sides() {
    // `x ^ x == 0` holds only if both sides constrain the same `x`.
    let mut lhs = SemGraph::<()>::new();
    let x = sym(&mut lhs, 0);
    op(&mut lhs, SymKind::Xor, &[x, x]);
    let mut rhs = SemGraph::new();
    con(&mut rhs, 0, 32);
    assert!(SmtOracle.equivalent(&lhs, &rhs, &[32]));
}

#[test]
fn smt_oracle_finds_counterexamples_over_two_symbols() {
    // `x + y != x - y` whenever `2*y != 0`.
    let mut lhs = SemGraph::<()>::new();
    let x = sym(&mut lhs, 0);
    let y = sym(&mut lhs, 1);
    op(&mut lhs, SymKind::Add, &[x, y]);
    let mut rhs = SemGraph::new();
    let x = sym(&mut rhs, 0);
    let y = sym(&mut rhs, 1);
    op(&mut rhs, SymKind::Sub, &[x, y]);
    assert!(!SmtOracle.equivalent(&lhs, &rhs, &[32, 32]));
}

#[test]
fn bool_via_if_identity_is_proved() {
    assert!(confirm_bool_via_if(&SmtOracle));
}
