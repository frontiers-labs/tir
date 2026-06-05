//! LIT-style FileCheck tests for target selection through `tir-mc`.

fn main() {
    let tir_mc = tir_lit::cargo_test_bin("tir-mc", "tir-mc");
    let tir_mc = tir_mc.to_str().expect("tir-mc path must be valid UTF-8");

    tir_lit::harness_main(env!("CARGO_MANIFEST_DIR"), "checks", &[("tir-mc", tir_mc)]);
}
