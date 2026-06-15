//! LIT-style FileCheck tests for ARM64 codegen.
//!
//! The tests live with the backend because they exercise ARM64 instruction
//! selection and register allocation through the `tir-mc` driver.

fn main() {
    let tir = tir_lit::cargo_test_bin("tir-tools", "tir");
    let tir = tir.to_str().expect("tir path must be valid UTF-8");

    tir_lit::harness_main(env!("CARGO_MANIFEST_DIR"), "checks", &[("tir", tir)]);
}
