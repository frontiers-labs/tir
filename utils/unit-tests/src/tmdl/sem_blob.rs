//! The sem programs a generated backend embeds live in a binary blob beside
//! the generated Rust, so these properties of behavior lowering are asserted
//! over the decoded blob rather than over the emitted source.

use tir_symbolic::lang::SymKind;

use super::support::{fixture, generate};

#[test]
fn a_let_binding_is_built_once_and_re_read_by_later_statements() {
    let programs = generate(&fixture("checks/Inputs/lets.tmdl"), "test").programs;

    // `let sum = rs1 + rs2` builds the sum from the two operand symbols once.
    assert!(programs.contains(&"Symbol#0 Symbol#1 Add 2<-0 2<-1".to_string()));
    // `rd = sum + sum` reads the binding's symbol instead of rebuilding it.
    assert!(programs.contains(&"Symbol#2 Add 1<-0 1<-0".to_string()));
    // `rd = sext(old, XLEN)` over a bound RMW extends the binding's symbol; the
    // RMW is not re-issued by the write.
    assert!(programs.contains(&"Symbol#2 Symbol#3 SExt 2<-0 2<-1".to_string()));
    // Likewise a bound load: the write adds and extends the binding's symbol.
    assert!(programs.contains(&"Symbol#1 Add 1<-0 1<-0 Symbol#2 SExt 3<-1 3<-2".to_string()));
    // A binding used verbatim as a statement's value is just its symbol.
    assert!(programs.contains(&"Symbol#2".to_string()));
}

#[test]
fn a_const_memory_size_is_folded_to_its_byte_count() {
    let generated = generate(&fixture("checks/Inputs/param-mem-size.tmdl"), "test");

    assert!(
        !generated.kinds.contains(&SymKind::Div),
        "self.XLEN / 8 is not folded"
    );
    assert!(generated
        .programs
        .iter()
        .any(|program| program.contains("8:4") && program.contains("LoadMemory")));
    assert!(generated
        .programs
        .iter()
        .any(|program| program.contains("8:4") && program.contains("StoreMemory")));
}

#[test]
fn each_atomic_construct_lowers_to_its_dedicated_kind() {
    let generated = generate(&fixture("checks/Inputs/atomics.tmdl"), "test");

    for kind in [
        SymKind::LoadReserved,
        SymKind::StoreConditional,
        SymKind::AtomicRmw,
        SymKind::Fence,
    ] {
        assert!(generated.kinds.contains(&kind), "{kind:?} is not lowered");
    }
    // A plain store with an explicit ordering packs the ordering code into the
    // inert metadata operand (`seq_cst` = 4 -> bits 3:1 -> value 8, width 4).
    assert!(generated
        .program_after("impl tir::sem::AsSemExpr for StRelOp")
        .contains("8:4"));
}

#[test]
fn a_regnum_guard_compares_the_captured_index_symbol() {
    let generated = generate(&fixture("checks/Rust/regnum.tmdl"), "test");

    assert!(generated
        .programs
        .iter()
        .any(|program| program.split(' ').any(|step| step == "Ne")));
}

#[test]
fn flag_reading_rules_compose_the_comparison_they_prove() {
    let generated = generate(&fixture("checks/Rust/flag_branches.tmdl"), "test");

    assert!(generated
        .rule_program("static RULE_BRANCHEQ_VIA_CMP")
        .contains("Eq"));
    assert!(generated
        .rule_program("static RULE_BRANCHLT_VIA_CMP")
        .contains("Lt"));
    assert!(generated
        .rule_program("static RULE_SELECTEQ_VIA_CMP")
        .contains("If"));
}
