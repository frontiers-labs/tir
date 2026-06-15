//! LIT-style FileCheck tests for RISC-V codegen.
//!
//! The tests live with the backend because they exercise RISC-V instruction
//! selection and register allocation through the `tir-mc` driver.

fn main() {
    let tir = tir_lit::cargo_test_bin("tir-tools", "tir");
    let tir = tir.to_str().expect("tir-mc path must be valid UTF-8");

    tir_lit::harness_main(env!("CARGO_MANIFEST_DIR"), "checks", &[("tir", tir)]);
}
