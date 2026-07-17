use std::process::Command;
use tir_tools::model_check::stitch;

const IMPLEMENTATION: &str = "\
1 sort bitvec 8
2 input 1 clk
3 state 1 reg
4 output 3 x
5 input 1 raw
6 output 5 y
";

const CHECKER: &str = "\
1 sort bitvec 8
2 input 1 x
3 input 1 y
4 neq 1 2 3
5 bad 4 mismatch
";

fn working_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("tir-model-check-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn wires_checker_inputs_to_dut_outputs() {
    let miter = stitch(IMPLEMENTATION, CHECKER, &["x", "y"]).unwrap();

    assert!(miter.contains("3 state 1 reg"));
    assert!(miter.contains("10 neq 7 3 5"), "{miter}");
    assert!(miter.contains("bad 10 mismatch"), "{miter}");
    assert!(!miter.contains("input 7 x"));
}

#[test]
fn reset_gates_properties() {
    let implementation = format!("{IMPLEMENTATION}7 input 1 reset\n");
    let miter = stitch(&implementation, CHECKER, &["x", "y"]).unwrap();

    assert!(miter.lines().any(|line| line.ends_with(" started")));
    assert!(miter.lines().any(|line| line.contains(" constraint ")));
    let bad = miter
        .lines()
        .rev()
        .find(|line| line.contains(" bad "))
        .unwrap();
    let gated = bad.split_whitespace().nth(2).unwrap();
    assert!(
        miter
            .lines()
            .any(|line| line.starts_with(&format!("{gated} and "))),
        "{miter}"
    );
}

#[test]
fn reports_a_missing_dut_signal() {
    let error = stitch(IMPLEMENTATION, CHECKER, &["x", "z"]).unwrap_err();

    assert!(error.to_string().contains("`z`"), "{error}");
}

#[test]
fn rv64g_checker_generation_does_not_panic() {
    let working_dir = working_dir("rv64g");
    let missing = std::env::temp_dir().join("tir-model-check-missing-dut.btor2");
    let output = Command::new(env!("CARGO_BIN_EXE_tir"))
        .args(["model-check", "--target=rv64g"])
        .arg(&missing)
        .current_dir(&working_dir)
        .output()
        .expect("run tir model-check");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to read DUT"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
    std::fs::remove_dir_all(working_dir).unwrap();
}

#[test]
fn checker_respects_target_extensions() {
    let working_dir = working_dir("features");
    let missing = std::env::temp_dir().join("tir-model-check-missing-dut.btor2");
    let output = Command::new(env!("CARGO_BIN_EXE_tir"))
        .args(["model-check", "--target=rv64i"])
        .arg(&missing)
        .current_dir(&working_dir)
        .output()
        .expect("run tir model-check");
    assert!(!output.status.success());

    let checker = working_dir.join("target/model-check/rv64i/checker.btor2");
    let checker = std::fs::read_to_string(checker).expect("read checker");
    assert!(checker.contains("; modeled Add\n"));
    assert!(!checker.contains("; modeled Mul\n"));
    std::fs::remove_dir_all(working_dir).unwrap();
}

#[test]
fn standalone_invocation_writes_artifacts_under_working_directory() {
    let working_dir = working_dir("standalone");
    let output = Command::new(env!("CARGO_BIN_EXE_tir"))
        .args(["model-check", "--target=rv64i", "missing.btor2"])
        .current_dir(&working_dir)
        .output()
        .expect("run standalone tir model-check");

    assert!(!output.status.success());
    assert!(
        working_dir
            .join("target/model-check/rv64i/checker.btor2")
            .is_file()
    );
    std::fs::remove_dir_all(working_dir).unwrap();
}

#[test]
fn arm64_checker_generation_reaches_dut_read() {
    let working_dir = working_dir("arm64");
    let output = Command::new(env!("CARGO_BIN_EXE_tir"))
        .args(["model-check", "--target=arm64", "missing.btor2"])
        .current_dir(&working_dir)
        .output()
        .expect("run tir model-check for arm64");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to read DUT"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
    let checker =
        std::fs::read_to_string(working_dir.join("target/model-check/arm64/checker.btor2"))
            .unwrap();
    assert!(checker.contains("; modeled Add\n"));
    assert!(checker.contains(" input ") && checker.contains(" src0_val\n"));
    std::fs::remove_dir_all(working_dir).unwrap();
}

#[test]
fn x86_64_checker_generation_reaches_dut_read() {
    let working_dir = working_dir("x86_64");
    let output = Command::new(env!("CARGO_BIN_EXE_tir"))
        .args(["model-check", "--target=x86_64", "missing.btor2"])
        .current_dir(&working_dir)
        .output()
        .expect("run tir model-check for x86_64");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to read DUT"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
    let checker =
        std::fs::read_to_string(working_dir.join("target/model-check/x86_64/checker.btor2"))
            .unwrap();
    assert!(checker.contains("; modeled Add\n"));
    assert!(!checker.contains("; modeled MovLoadSib\n"));
    std::fs::remove_dir_all(working_dir).unwrap();
}
