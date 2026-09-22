use std::fs;
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

fn check_fixture(
    expectation: &str,
    observation: &str,
    reference_status: &str,
    stage: &str,
) -> (bool, serde_json::Value) {
    check_fixture_with_compiler(expectation, observation, reference_status, stage, "15.2.0")
}

fn check_fixture_with_compiler(
    expectation: &str,
    observation: &str,
    reference_status: &str,
    stage: &str,
    compiler_version: &str,
) -> (bool, serde_json::Value) {
    check_fixture_with_provenance(
        expectation,
        observation,
        reference_status,
        stage,
        compiler_version,
        None,
        None,
    )
}

fn check_fixture_with_provenance(
    expectation: &str,
    observation: &str,
    reference_status: &str,
    stage: &str,
    compiler_version: &str,
    recorded_source_digest: Option<&str>,
    recorded_manifest_digest: Option<&str>,
) -> (bool, serde_json::Value) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("probe.c");
    let manifest = directory.path().join("cases.toml");
    let reference = directory.path().join("reference.json");
    let checked = directory.path().join("checked.json");
    let source_contents = b"int main(void) { return 0; }\n";
    fs::write(&source, source_contents).unwrap();
    let mut source_digest = Sha256::new();
    source_digest.input(source_contents);
    let actual_source_digest = format!("sha256:{:x}", source_digest.result());
    let source_digest = recorded_source_digest.unwrap_or(&actual_source_digest);
    fs::write(
        &manifest,
        format!(
            r#"schema_version = 1

[reference]
profile = "gcc-15.2"
compiler_version = "15.2.0"

[[cases]]
id = "fixture.case"
stage = "{stage}"
source = "{}"
language_mode = "c17"
target_requirements = []
compiler_args = []
runtime_input_bits = []
probe = "execute"

[cases.expectation]
{expectation}

[cases.oracle]
kind = "fixture"
identity = "fixture"
version = "1"
reference = "fixture"
"#,
            source.display(),
        ),
    )
    .unwrap();
    let manifest_contents = fs::read(&manifest).unwrap();
    let mut manifest_digest = Sha256::new();
    manifest_digest.input(&manifest_contents);
    let actual_manifest_digest = format!("sha256:{:x}", manifest_digest.result());
    let manifest_digest = recorded_manifest_digest.unwrap_or(&actual_manifest_digest);
    fs::write(
        &reference,
        format!(
            r#"{{
  "schema_version": 1,
  "manifest_digest": "{manifest_digest}",
  "profile": "gcc-15.2",
  "generated_at_unix_seconds": 0,
  "host": {{ "target": "x86_64-linux-gnu", "library": "glibc 2.43" }},
  "results": [{{
    "case_id": "fixture.case",
    "stage": "{stage}",
    "compiler": {{ "version": "{compiler_version}", "executable": "/usr/bin/gcc" }},
    "source_digest": "{source_digest}",
    "commands": [["/usr/bin/gcc", "probe.c"]],
    "exit_status": 0,
    "observation": {observation},
    "status": "{reference_status}",
    "detail": "fixture"
  }}]
}}"#,
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "fp-check",
            "check",
            "--stage",
            stage,
            "--manifest",
            manifest.to_str().unwrap(),
            "--reference",
            reference.to_str().unwrap(),
            "--output",
            checked.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let checked = serde_json::from_str(&fs::read_to_string(checked).unwrap()).unwrap();
    (output.status.success(), checked)
}

#[test]
fn check_rejects_stale_compiler_provenance() {
    let (success, report) = check_fixture_with_compiler(
        "kind = \"exact_bits\"\nbits = \"0x0\"\nflags = []",
        r#"{"kind":"exact_bits","bits":"0x0","flags":[]}"#,
        "pass",
        "reference",
        "0.0.0",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert!(report["results"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("compiler version"));
}

#[test]
fn check_rejects_stale_source_provenance() {
    let (success, report) = check_fixture_with_provenance(
        "kind = \"exact_bits\"\nbits = \"0x0\"\nflags = []",
        r#"{"kind":"exact_bits","bits":"0x0","flags":[]}"#,
        "pass",
        "reference",
        "15.2.0",
        Some("sha256:stale"),
        None,
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert!(report["results"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("source digest"));
}

#[test]
fn check_rejects_stale_case_configuration() {
    let (success, report) = check_fixture_with_provenance(
        "kind = \"exact_bits\"\nbits = \"0x0\"\nflags = []",
        r#"{"kind":"exact_bits","bits":"0x0","flags":[]}"#,
        "pass",
        "reference",
        "15.2.0",
        None,
        Some("sha256:stale"),
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert!(report["results"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("manifest digest"));
}

#[test]
fn report_rejects_a_wrong_result_bit() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report.json");
    fs::write(
        &report,
        r#"{
  "schema_version": 1,
  "profile": "gcc-15.2",
  "generated_at_unix_seconds": 0,
  "host": {
    "target": "x86_64-linux-gnu",
    "library": "glibc 2.43"
  },
  "results": [
    {
      "case_id": "fma.binary64.value.separate",
      "stage": "reference",
      "compiler": {
        "version": "gcc 15.2.0",
        "executable": "/usr/bin/gcc"
      },
      "source_digest": "sha256:test",
      "commands": [["gcc", "probe.c"]],
      "exit_status": 0,
      "observation": {
        "kind": "exact_bits",
        "bits": "0x8000000000000000",
        "flags": []
      },
      "status": "fail",
      "detail": "expected 0x0000000000000000, observed 0x8000000000000000"
    }
  ]
}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["fp-check", "report", report.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("fail=1"),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn report_with_effects_observation(observation: &str) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report.json");
    fs::write(
        &report,
        format!(
            r#"{{
  "schema_version": 1,
  "profile": "gcc-15.2",
  "generated_at_unix_seconds": 0,
  "host": {{ "target": "x86_64-linux-gnu", "library": "glibc 2.43" }},
  "results": [{{
    "case_id": "effects.fixture",
    "stage": "reference",
    "compiler": {{ "version": "15.2.0", "executable": "/usr/bin/gcc" }},
    "source_digest": "sha256:test",
    "commands": [["/usr/bin/gcc", "probe.c"]],
    "exit_status": 0,
    "observation": {observation},
    "status": "pass",
    "detail": "fixture"
  }}]
}}"#,
        ),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["fp-check", "report", report.to_str().unwrap()])
        .output()
        .unwrap()
}

#[test]
fn report_rejects_effects_observations_without_flags_or_events() {
    for field in ["flags", "events"] {
        let mut observation = serde_json::json!({
            "kind": "effects", "flags": [], "events": [], "errno": null, "trapped": false
        });
        observation.as_object_mut().unwrap().remove(field);
        let output = report_with_effects_observation(&observation.to_string());
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("missing field `{field}`")),
            "stderr:\n{stderr}"
        );
    }
}

#[test]
fn report_accepts_unknown_effects_observation_fields() {
    let output = report_with_effects_observation(
        r#"{"kind":"effects","flags":[],"errno":null,"events":[],"trapped":false,"extension":true}"#,
    );

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_detects_a_wrong_result_bit() {
    let (success, report) = check_fixture(
        "kind = \"exact_bits\"\nbits = \"0x0000000000000000\"\nflags = []",
        r#"{"kind":"exact_bits","bits":"0x0000000000000001","flags":[]}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert_eq!(
        report["results"][0]["detail"],
        "expected bits 0x0000000000000000, observed 0x0000000000000001"
    );
}

#[test]
fn check_detects_a_wrong_zero_sign() {
    let (success, report) = check_fixture(
        "kind = \"exact_bits\"\nbits = \"0x0000000000000000\"\nflags = []",
        r#"{"kind":"exact_bits","bits":"0x8000000000000000","flags":[]}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert_eq!(
        report["results"][0]["detail"],
        "expected bits 0x0000000000000000, observed 0x8000000000000000"
    );
}

#[test]
fn check_detects_a_missing_flag() {
    let (success, report) = check_fixture(
        "kind = \"exact_bits\"\nbits = \"0x0000000000000000\"\nflags = [\"inexact\"]",
        r#"{"kind":"exact_bits","bits":"0x0000000000000000","flags":[]}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert!(report["results"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("missing flags [\"inexact\"]"));
}

#[test]
fn check_detects_a_missing_errno_update() {
    let (success, report) = check_fixture(
        "kind = \"effects\"\nflags = []\nerrno = 34\nevents = []\ntrapped = false",
        r#"{"kind":"effects","flags":[],"errno":123,"events":[],"trapped":false}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert_eq!(
        report["results"][0]["detail"],
        "expected errno Some(34), observed Some(123)"
    );
}

#[test]
fn check_detects_an_unexpected_trap() {
    let (success, report) = check_fixture(
        "kind = \"effects\"\nflags = []\nevents = []\ntrapped = false",
        r#"{"kind":"effects","flags":[],"errno":null,"events":[],"trapped":true}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert_eq!(
        report["results"][0]["detail"],
        "expected trapped=false, observed trapped=true"
    );
}

#[test]
fn check_detects_a_wrong_vector_lane_result() {
    let (success, report) = check_fixture(
        "kind = \"effects\"\nresult_bits = [\"0x3ff0000000000000\", \"0x4000000000000000\"]\nflags = []\nevents = [\"sqrt:lane1\"]\ntrapped = false",
        r#"{"kind":"effects","result_bits":["0x3ff0000000000000","0x0000000000000000"],"flags":[],"errno":null,"events":["sqrt:lane1"],"trapped":false}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert!(report["results"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("result bits"));
}

#[test]
fn check_detects_an_absent_instruction() {
    let (success, report) = check_fixture(
        "kind = \"code_shape\"\nrequired = [\"vfmadd\"]\nforbidden = []",
        r#"{"kind":"code_shape","instructions":["vmulsd %xmm1, %xmm0, %xmm0"]}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert_eq!(
        report["results"][0]["detail"],
        "required instruction vfmadd is absent"
    );
}

#[test]
fn check_preserves_an_unsupported_capability() {
    let (success, report) = check_fixture(
        "kind = \"exact_bits\"\nbits = \"0x0000000000000000\"\nflags = []",
        "null",
        "unsupported_capability",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "unsupported_capability");
    assert_eq!(report["results"][0]["detail"], "fixture");
}

#[test]
fn check_preserves_missing_infrastructure() {
    let (success, report) = check_fixture(
        "kind = \"exact_bits\"\nbits = \"0x0000000000000000\"\nflags = []",
        "null",
        "missing_infrastructure",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "missing_infrastructure");
    assert_eq!(report["results"][0]["detail"], "fixture");
}

#[test]
fn check_detects_a_numerical_error_above_the_bound() {
    let (success, report) = check_fixture(
        "kind = \"numerical_bound\"\nmax_error = \"1e-12\"\nmetric = \"relative\"\ndomain = \"[0.5, 2.0]\"\nzero_convention = \"excluded\"\nsubnormal_convention = \"relative\"\nexceptional_values = \"excluded\"",
        r#"{"kind":"numerical_bound","value":"1.0","error":"2e-12"}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert_eq!(report["results"][0]["detail"], "error 2e-12 exceeds 1e-12");
}

#[test]
fn check_requires_all_correlated_results() {
    let (success, report) = check_fixture(
        "kind = \"correlated_results\"\nresults = [\"strict:0x0\", \"relaxed:0x1\"]",
        r#"{"kind":"correlated_results","results":["relaxed:0x1"]}"#,
        "pass",
        "reference",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "fail");
    assert!(report["results"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("correlated results"));
}

#[test]
fn check_does_not_count_future_gcc_evidence_as_tir_support() {
    let (success, report) = check_fixture(
        "kind = \"exact_bits\"\nbits = \"0x0000000000000000\"\nflags = []",
        r#"{"kind":"exact_bits","bits":"0x0000000000000000","flags":[]}"#,
        "pass",
        "scalar",
    );

    assert!(!success);
    assert_eq!(report["results"][0]["status"], "unsupported_capability");
    assert_eq!(report["results"][0]["stage"], "scalar");
}

fn check_preserved_probe(contents: &str) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("probe.c");
    let manifest = directory.path().join("cases.toml");
    let report = directory.path().join("reference.json");
    fs::write(&source, contents).unwrap();
    fs::write(
        &manifest,
        format!(
            r#"schema_version = 1

[reference]
profile = "gcc-15.2"
compiler_version = "15.2.0"

[[cases]]
id = "failed.probe"
stage = "reference"
source = "{}"
language_mode = "c17"
target_requirements = []
compiler_args = []
runtime_input_bits = []
probe = "execute"

[cases.expectation]
kind = "exact_bits"
bits = "0x0"
flags = []

[cases.oracle]
kind = "fixture"
identity = "fixture"
version = "1"
reference = "fixture"
"#,
            source.display()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "fp-check",
            "reference",
            "--gcc",
            "gcc",
            "--profile",
            "host-test",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            report.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
    assert_eq!(report["results"][0]["status"], "fail");
    let artifacts = report["results"][0]["artifacts"].as_str().unwrap();
    assert!(std::path::Path::new(artifacts).join("probe.c").is_file());
    fs::remove_dir_all(artifacts).unwrap();
}

#[test]
fn reference_preserves_a_malformed_probe() {
    check_preserved_probe("#include <stdio.h>\nint main(void) { puts(\"not json\"); return 0; }\n");
}

#[test]
fn reference_preserves_a_probe_that_misses_its_expectation() {
    check_preserved_probe(
        "#include <stdio.h>\nint main(void) { puts(\"{\\\"kind\\\":\\\"exact_bits\\\",\\\"bits\\\":\\\"0x1\\\",\\\"flags\\\":[]}\"); return 0; }\n",
    );
}

#[test]
fn reference_detects_a_store_moved_before_a_trap() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("probe.c");
    let manifest = directory.path().join("cases.toml");
    let report = directory.path().join("reference.json");
    let original = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fcc/checks/Inputs/fp/reference/effects.c"),
    )
    .unwrap();
    let moved = original.replace(
        "double result = one / zero;\n    (void)result;\n    event_state = 3;",
        "event_state = 3;\n    double result = one / zero;\n    (void)result;",
    );
    assert_ne!(moved, original);
    fs::write(&source, moved).unwrap();
    fs::write(
        &manifest,
        format!(
            r#"schema_version = 1

[reference]
profile = "gcc-15.2"
compiler_version = "15.2.0"

[[cases]]
id = "trap.moved-store"
stage = "reference"
source = "{}"
language_mode = "gnu17"
target_requirements = ["libm"]
compiler_args = ["-O0", "-frounding-math", "-ftrapping-math"]
runtime_input_bits = []
run_args = ["trap"]
probe = "execute"

[cases.expectation]
kind = "effects"
flags = []
events = ["store_before", "trap"]
trapped = true

[cases.oracle]
kind = "fixture"
identity = "fixture"
version = "1"
reference = "fixture"
"#,
            source.display()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "fp-check",
            "reference",
            "--gcc",
            "gcc",
            "--profile",
            "host-test",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            report.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
    assert_eq!(report["results"][0]["status"], "fail");
    assert_eq!(
        report["results"][0]["observation"]["events"],
        serde_json::json!(["store_before", "store_after", "trap"])
    );
    let artifacts = report["results"][0]["artifacts"].as_str().unwrap();
    fs::remove_dir_all(artifacts).unwrap();
}

#[test]
fn check_accepts_the_fma_reference_bits_and_flags() {
    let directory = tempfile::tempdir().unwrap();
    let reference = directory.path().join("reference.json");
    let output_path = directory.path().join("checked.json");
    fs::write(
        &reference,
        r#"{
  "schema_version": 1,
  "profile": "gcc-15.2",
  "generated_at_unix_seconds": 0,
  "host": {
    "target": "x86_64-linux-gnu",
    "library": "glibc 2.43"
  },
  "results": [
    {
      "case_id": "fma.binary64.value.separate",
      "stage": "reference",
      "compiler": { "version": "15.2.0", "executable": "/usr/bin/gcc" },
      "source_digest": "sha256:ab2f986627ae1e2caf008e3845c1dbfceed5e386904dd525513a594c47190e7d",
      "commands": [["/usr/bin/gcc", "fma.c", "separate"]],
      "exit_status": 0,
      "observation": { "kind": "exact_bits", "bits": "0x0000000000000000", "flags": ["inexact"] },
      "status": "pass",
      "detail": "matches exact derivation"
    },
    {
      "case_id": "fma.binary64.value.fused",
      "stage": "reference",
      "compiler": { "version": "15.2.0", "executable": "/usr/bin/gcc" },
      "source_digest": "sha256:ab2f986627ae1e2caf008e3845c1dbfceed5e386904dd525513a594c47190e7d",
      "commands": [["/usr/bin/gcc", "fma.c", "fused"]],
      "exit_status": 0,
      "observation": { "kind": "exact_bits", "bits": "0xbc90000000000000", "flags": [] },
      "status": "pass",
      "detail": "matches exact derivation"
    }
  ]
}"#,
    )
    .unwrap();
    let manifest =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fcc/checks/Inputs/fp/cases.toml");
    let mut manifest_digest = Sha256::new();
    manifest_digest.input(fs::read(manifest).unwrap());
    let manifest_digest = format!("sha256:{:x}", manifest_digest.result());
    let mut reference_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&reference).unwrap()).unwrap();
    reference_json["manifest_digest"] = serde_json::json!(manifest_digest);
    fs::write(
        &reference,
        serde_json::to_vec_pretty(&reference_json).unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "fp-check",
            "check",
            "--stage",
            "reference",
            "--reference",
            reference.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let checked: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output_path).unwrap()).unwrap();
    assert_eq!(checked["results"][0]["status"], "pass");
    assert_eq!(checked["results"][1]["status"], "pass");
}

#[test]
fn reference_records_gcc_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("reference.json");
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "fp-check",
            "reference",
            "--gcc",
            "gcc",
            "--profile",
            "host-test",
            "--case",
            "fma.binary64.value.fused",
            "--output",
            report.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
    assert_eq!(report["profile"], "host-test");
    assert!(!report["host"]["target"].as_str().unwrap().is_empty());
    assert!(report["host"]["library"]
        .as_str()
        .unwrap()
        .starts_with("glibc "));
    assert!(!report["results"][0]["compiler"]["version"]
        .as_str()
        .unwrap()
        .is_empty());
    assert!(report["results"][0]["compiler"]["executable"]
        .as_str()
        .unwrap()
        .contains("gcc"));
    assert!(report["results"][0]["source_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(report["results"][0]["commands"].is_array());
    assert_eq!(
        report["results"][0]["observation"]["bits"],
        "0xbc90000000000000"
    );
    assert_eq!(report["results"][0]["status"], "pass");
}

#[test]
fn reference_rejects_a_missing_compiler() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("reference.json");
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "fp-check",
            "reference",
            "--gcc",
            "/definitely/missing/gcc",
            "--output",
            report.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("/definitely/missing/gcc"));
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap();
    assert!(!report["results"].as_array().unwrap().is_empty());
    assert!(report["results"].as_array().unwrap().iter().all(|result| {
        result["status"] == "missing_infrastructure"
            && result["detail"]
                .as_str()
                .unwrap()
                .contains("/definitely/missing/gcc")
    }));
}

#[test]
fn check_rejects_an_empty_case_selection() {
    let directory = tempfile::tempdir().unwrap();
    let reference = directory.path().join("reference.json");
    let checked = directory.path().join("checked.json");
    fs::write(
        &reference,
        r#"{
  "schema_version": 1,
  "profile": "gcc-15.2",
  "generated_at_unix_seconds": 0,
  "host": { "target": "x86_64-linux-gnu", "library": "glibc 2.43" },
  "results": []
}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "fp-check",
            "check",
            "--stage",
            "reference",
            "--case",
            "missing.case",
            "--reference",
            reference.to_str().unwrap(),
            "--output",
            checked.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no cases selected"));
    assert!(!checked.exists());
}

fn reference_case(case: &str) -> serde_json::Value {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("reference.json");
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "fp-check",
            "reference",
            "--gcc",
            "gcc",
            "--profile",
            "host-test",
            "--case",
            case,
            "--output",
            report.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{case}: stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(&fs::read_to_string(report).unwrap()).unwrap()
}

#[test]
fn reference_records_same_expression_contraction() {
    let report = reference_case("contraction.gnu17.same.default");
    assert_eq!(report["results"][0]["status"], "pass");
    assert!(report["results"][0]["observation"]["instructions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line.as_str().unwrap().contains("vfmadd")));
}

#[test]
fn reference_records_underflow_for_minimum_subnormal_scaling() {
    let report = reference_case("scale.binary64.positive_min_subnormal");
    assert_eq!(
        report["results"][0]["observation"]["bits"],
        "0x0000000000000000"
    );
    assert_eq!(
        report["results"][0]["observation"]["flags"],
        serde_json::json!(["underflow", "inexact"])
    );
}

#[test]
fn reference_records_directed_halfway_rounding() {
    let report = reference_case("round.binary64.halfway.upward");
    assert_eq!(
        report["results"][0]["observation"]["bits"],
        "0x3ff0000000000001"
    );
    assert_eq!(
        report["results"][0]["observation"]["flags"],
        serde_json::json!(["inexact"])
    );
}

#[test]
fn reference_records_observable_dead_arithmetic() {
    let report = reference_case("effects.dead_division.flags");
    assert_eq!(
        report["results"][0]["observation"]["flags"],
        serde_json::json!([])
    );
    assert_eq!(report["results"][0]["observation"]["errno"], 123);
    assert_eq!(report["results"][0]["status"], "pass");
}

#[test]
fn reference_records_negative_sqrt_reporting() {
    let report = reference_case("math.sqrt.negative.glibc");
    assert_eq!(
        report["results"][0]["observation"]["flags"],
        serde_json::json!(["invalid"])
    );
    assert_eq!(report["results"][0]["observation"]["errno"], 33);
    assert_eq!(
        report["results"][0]["observation"]["result_bits"],
        "0xfff8000000000000"
    );
}

#[test]
fn reference_records_disabled_builtin_recognition() {
    let report = reference_case("recognition.sqrt.builtin_disabled");
    assert!(report["results"][0]["observation"]["instructions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line.as_str().unwrap().contains("sqrt@PLT")));
}

#[test]
fn reference_records_the_mixed_policy_inline_boundary() {
    let report = reference_case("scope.fp_contract.mixed_inline");
    assert_eq!(report["results"][0]["status"], "pass");
    assert!(report["results"][0]["observation"]["instructions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line.as_str().unwrap().contains("call\tstrict_inline")));
}

#[test]
fn reference_accepts_the_expected_declaration_diagnostic() {
    let report = reference_case("recognition.sqrt.declaration_mismatch");
    assert_eq!(report["results"][0]["status"], "pass");
    assert!(report["results"][0]["observation"]["message"]
        .as_str()
        .unwrap()
        .contains("conflicting types for"));
}

#[test]
fn reference_records_reserved_cases_as_unsupported() {
    let report = reference_case("vector.sqrt.inactive_lane");
    assert_eq!(report["results"][0]["status"], "unsupported_capability");
    assert!(report["results"][0]["observation"].is_null());
}
