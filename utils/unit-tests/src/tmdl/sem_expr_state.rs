use super::support::generate_source;

const WRITEBACK_STORE: &str = r#"
isa Test { param XLEN: Integer = 32; }

register_class GPR for [Test] {
    param ENCODING_LEN: Integer = 5;
    param WIDTH: Integer = self.XLEN;
    registers { r0..r31 => {}, }
}

instruction StoreInc for [Test] {
    param MNEMONIC: String = "storeinc";
    operands { rn: GPR, rt: GPR, }
    asm { "{self.MNEMONIC} {rt}, ({rn})" }
    behavior {
        rn = rn + zext(0b1, self.XLEN);
        store(rn, 4, rt);
    }
}
"#;

#[test]
fn later_reads_use_named_writeback_without_changing_other_operands() {
    let generated = generate_source("writeback-store.tmdl", WRITEBACK_STORE, "test");

    // The store after `rn = rn + ...` addresses the written-back value (the
    // updated expression, not a stale re-read of `rn`) while its stored value
    // stays `rt`'s own symbol.
    let store = generated
        .programs
        .iter()
        .find(|program| program.contains("StoreMemory"))
        .expect("a store program is emitted");
    assert!(
        store.contains("Add") && store.contains("Symbol"),
        "store must address the written-back expression: {store}"
    );
}
