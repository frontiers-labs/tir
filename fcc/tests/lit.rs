//! LIT-style FileCheck tests for the `fcc` C compiler.
//!
//! Every file under `fcc/checks` that contains a `RUN:` line runs as an
//! individual `cargo test` case, driving the `fcc` binary built by Cargo and
//! verifying its output with the in-process `filecheck` matcher. Golden tests
//! (preprocessor/lexer/ast) are regenerated with
//! `./utils/scripts/update_checks.py fcc`; the `Codegen` tests are authored by
//! hand.

fn main() {
    tir_lit::harness_main_with_tools(
        env!("CARGO_MANIFEST_DIR"),
        "checks",
        &[
            ("fcc", tir_lit::Tool::path(env!("CARGO_BIN_EXE_fcc"))),
            ("tir", tir_lit::Tool::cargo_test_bin("tir-tools", "tir")),
        ],
    );
}
