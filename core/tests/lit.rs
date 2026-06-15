//! LIT-style FileCheck tests for core IR passes.

fn main() {
    let tir = tir_lit::cargo_test_bin("tir-tools", "tir");
    let tir = tir
        .to_str()
        .expect("tir-opt wrapper path must be valid UTF-8");

    tir_lit::harness_main(env!("CARGO_MANIFEST_DIR"), "checks", &[("tir", tir)]);
}
