use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn fcc_binary() -> &'static Path {
    static FCC: OnceLock<PathBuf> = OnceLock::new();
    FCC.get_or_init(|| tir_lit::cargo_test_bin("fcc", "fcc"))
}

fn validator() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fcc/checks/Inputs/validate_pbqp_dumps.py")
}

fn source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fcc/checks/Inputs/pbqp_dump.c")
}

fn run_validator(scenario: &str) {
    let output = Command::new("python3")
        .arg(validator())
        .arg(scenario)
        .arg(fcc_binary())
        .arg(source())
        .output()
        .expect("run PBQP dump validator");

    assert!(
        output.status.success(),
        "validator failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn writes_valid_isel_and_regalloc_dumps() {
    run_validator("capture");
}

#[test]
fn unset_and_empty_dump_directories_disable_dumping() {
    run_validator("disabled");
}

#[test]
fn dumping_does_not_change_compiler_output() {
    run_validator("output");
}

#[test]
fn later_compilations_preserve_existing_dumps() {
    run_validator("preserve");
}

#[test]
fn invalid_dump_path_warns_without_failing_compilation() {
    run_validator("invalid-path");
}

#[test]
fn concurrent_compilers_write_separate_dumps() {
    run_validator("concurrent");
}
