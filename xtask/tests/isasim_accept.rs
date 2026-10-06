mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

struct Workspace {
    directory: tir_adt::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let directory = tir_adt::TempDir::new().unwrap();
        let root = directory.path().join("workspace");
        let fixtures = Path::new("xtask/tests/fixtures/isasim-accept");
        std::fs::create_dir_all(root.join(fixtures)).unwrap();
        for name in ["passing.toml", "skipped-required.toml"] {
            std::fs::copy(
                common::manifest_dir()
                    .join("tests/fixtures/isasim-accept")
                    .join(name),
                root.join(fixtures).join(name),
            )
            .unwrap();
        }
        let workspace = Self { directory, root };
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        ] {
            let output = Command::new("git")
                .args(args)
                .current_dir(&workspace.root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        workspace
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn manifest(&self, name: &str) -> PathBuf {
        self.root
            .join("xtask/tests/fixtures/isasim-accept")
            .join(name)
    }

    fn command(&self, binary: impl AsRef<Path>) -> Command {
        let mut command = Command::new(binary.as_ref());
        command
            .env("CARGO_MANIFEST_DIR", self.root.join("xtask"))
            .current_dir(&self.root);
        command
    }
}

#[test]
fn report_records_padded_lit_test_names() {
    let directory = Workspace::new();
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

    let output = directory
        .command(common::xtask())
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
    let directory = Workspace::new();
    let xtask = directory.path().join("xtask");
    std::fs::copy(common::xtask(), &xtask).unwrap();
    let report_path = directory.path().join("report.json");
    let manifest = directory.manifest("passing.toml");

    let output = directory
        .command(xtask)
        .args([
            "isasim-accept",
            "m01",
            "--manifest",
            manifest.to_str().unwrap(),
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
    let directory = Workspace::new();
    let report = directory.path().join("report.json");
    let manifest = directory.manifest("skipped-required.toml");

    let output = directory
        .command(common::xtask())
        .args([
            "isasim-accept",
            "m01",
            "--strict",
            "--manifest",
            manifest.to_str().unwrap(),
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
    let directory = Workspace::new();
    let report_path = directory.path().join("report.json");
    let manifest = directory.manifest("passing.toml");
    let xtask = directory.path().join("xtask");
    std::fs::copy(common::xtask(), &xtask).unwrap();
    std::fs::write(directory.path().join("tir-isasim"), b"simulator fixture\n").unwrap();

    let output = directory
        .command(xtask)
        .args([
            "isasim-accept",
            "m01",
            "--manifest",
            manifest.to_str().unwrap(),
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
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&directory.root)
        .output()
        .unwrap();
    assert!(revision.status.success());
    assert_eq!(
        report["revision"],
        String::from_utf8(revision.stdout).unwrap().trim()
    );
    assert_eq!(
        report["workspace_state_sha256"],
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert!(report["runner_sha256"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(
        report["simulator_sha256"],
        "sha256:d95715a28eacd256a3c653dedc68e67b18471159ed503355c440d9a9847659ab"
    );
    assert!(directory.path().join("manifest.toml").is_file());
}
