//! Shared helpers for the fcc end-to-end link tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// The `fcc` binary under test, built once per process.
fn fcc_binary() -> &'static Path {
    static FCC: OnceLock<PathBuf> = OnceLock::new();
    FCC.get_or_init(|| tir_lit::cargo_test_bin("fcc", "fcc"))
}

pub fn cc_available() -> bool {
    Command::new("cc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn run_fcc(dir: &Path, args: &[&str]) {
    let status = Command::new(fcc_binary())
        .args(args)
        .current_dir(dir)
        .status()
        .expect("spawn fcc");
    assert!(status.success(), "fcc {args:?} failed");
}

pub fn run_program(dir: &Path, program: &str) -> Output {
    Command::new(dir.join(program))
        .output()
        .expect("run linked program")
}

pub fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("program exited via signal")
}

pub fn compile_fcc(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("test.c"), source).unwrap();
    run_fcc(dir, &["cc", "test.c", "-o", output]);
}

pub fn compile_host(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("host.c"), source).unwrap();
    let status = Command::new("cc")
        .args(["host.c", "-o", output])
        .current_dir(dir)
        .status()
        .expect("spawn host cc");
    assert!(status.success(), "host cc failed");
}

pub fn assert_fcc_matches_host(source: &str) {
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "fcc-program");
    compile_host(dir.path(), source, "host-program");
    let fcc = run_program(dir.path(), "fcc-program");
    let host = run_program(dir.path(), "host-program");
    assert_eq!(exit_code(&fcc), exit_code(&host));
    assert_eq!(fcc.stdout, host.stdout);
    assert_eq!(fcc.stderr, host.stderr);
}

pub fn compile_host_object(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("host.c"), source).unwrap();
    let status = Command::new("cc")
        .args(["-c", "host.c", "-o", output])
        .current_dir(dir)
        .status()
        .expect("spawn host cc");
    assert!(status.success(), "host cc failed");
}

pub fn assert_fcc_object_executes_with_host(source: &str, host: &str) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fcc.c"), source).unwrap();
    run_fcc(dir.path(), &["cc", "-c", "fcc.c", "-o", "fcc.o"]);
    compile_host_object(dir.path(), host, "host.o");
    run_fcc(dir.path(), &["cc", "fcc.o", "host.o", "-o", "program"]);
    assert_eq!(exit_code(&run_program(dir.path(), "program")), 0);
}

/// The compiler must be reproducible: the same input compiled by separate
/// processes must produce byte-identical assembly.
pub fn compile_asm(source: &Path) -> String {
    compile_asm_with_pipeline(source, None)
}

pub fn compile_asm_with_pipeline(source: &Path, pipeline: Option<&str>) -> String {
    let mut args = vec!["compile", "--march", "x86_64", "--stage", "asm", "-o", "-"];
    if let Some(pipeline) = pipeline {
        args.extend(["--pipeline", pipeline]);
    }
    let output = Command::new(fcc_binary())
        .args(args)
        .arg(source)
        .output()
        .expect("fcc should run");
    assert!(
        output.status.success(),
        "fcc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("assembly should be UTF-8")
}
