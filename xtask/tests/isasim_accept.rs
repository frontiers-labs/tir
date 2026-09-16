use std::process::Command;

#[test]
fn report_records_padded_lit_test_names() {
    let directory = tempfile::tempdir().unwrap();
    let report_path = directory.path().join("report.json");
    let manifest_path = directory.path().join("manifest.toml");
    std::fs::write(
        &manifest_path,
        r#"schema_version = 1
milestone = "m01"

[[cases]]
name = "padded-lit-output"
required = true
command = ["sh", "-c", "printf 'test simulator/isasim/checks/one.S             ... ok\\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'"]
tool = "sh"
expected_cases = 1
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "isasim-accept",
            "m01",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--report",
            report_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
    assert_eq!(
        report["results"][0]["test_names"][0],
        "simulator/isasim/checks/one.S"
    );
}

#[test]
fn report_marks_an_unavailable_simulator_binary() {
    let directory = tempfile::tempdir().unwrap();
    let xtask = directory.path().join("xtask");
    std::fs::copy(env!("CARGO_BIN_EXE_xtask"), &xtask).unwrap();
    let report_path = directory.path().join("report.json");
    let manifest = format!(
        "{}/tests/fixtures/isasim-accept/passing.toml",
        env!("CARGO_MANIFEST_DIR")
    );

    let output = Command::new(xtask)
        .args([
            "isasim-accept",
            "m01",
            "--manifest",
            &manifest,
            "--report",
            report_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
    assert!(report["simulator_sha256"].is_null());
}

#[test]
fn strict_mode_rejects_a_skipped_required_case() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report.json");
    let manifest = format!(
        "{}/tests/fixtures/isasim-accept/skipped-required.toml",
        env!("CARGO_MANIFEST_DIR")
    );

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "isasim-accept",
            "m01",
            "--strict",
            "--manifest",
            &manifest,
            "--report",
            report.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(report["summary"]["skipped"], 1);
    assert_eq!(report["results"][0]["status"], "skipped");
}

#[test]
fn report_records_provenance_and_optional_unsupported_cases() {
    let directory = tempfile::tempdir().unwrap();
    let report_path = directory.path().join("report.json");
    let manifest = format!(
        "{}/tests/fixtures/isasim-accept/passing.toml",
        env!("CARGO_MANIFEST_DIR")
    );
    let build = Command::new("cargo")
        .args(["build", "-p", "tir-isasim"])
        .output()
        .unwrap();
    assert!(build.status.success());

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "isasim-accept",
            "m01",
            "--manifest",
            &manifest,
            "--report",
            report_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(report["delivered"], false);
    assert_eq!(report["summary"]["passed"], 1);
    assert_eq!(report["summary"]["unsupported"], 1);
    assert_eq!(report["results"][0]["cases_passed"], 2);
    assert_eq!(report["results"][0]["test_names"][0], "fixture::one");
    assert!(directory.path().join("passing-required.log").is_file());
    assert!(report["results"][0]["output_sha256"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(report["manifest_sha256"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(
        report["fixture_sha256"]["xtask/tests/fixtures/isasim-accept/passing.toml"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(report["tool_versions"]["cargo"]
        .as_str()
        .unwrap()
        .starts_with("cargo "));
    assert!(report["tool_versions"]["rustc"]
        .as_str()
        .unwrap()
        .starts_with("rustc "));
    assert!(report["workspace_state_sha256"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(report["runner_sha256"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(report["simulator_sha256"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(directory.path().join("manifest.toml").is_file());
}
