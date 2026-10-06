//! Workspace-wide LIT driver.
//!
//! Runs every test suite in the workspace: each directory holding a
//! `test_suite.toml` contributes the FileCheck tests its globs select, named
//! by their path relative to the workspace root (e.g.
//! `backends/riscv/checks/GISel/add.tir`). Set `LIT_FILTER` to a regex to run
//! only matching tests, `LIT_FILTER_OUT` to exclude matching tests.

fn main() {
    tir_lit::workspace_harness_main();
}
