use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context};
use serde::Deserialize;
use xshell::{cmd, Shell};

use crate::lint::{LintDiagnostic, LintRule, Severity, Tool};

pub struct Jevlint;

const VERSION: &str = "v0.4.0-tir.1";

#[derive(Deserialize)]
struct Report {
    #[serde(deserialize_with = "Option::deserialize")]
    findings: Option<Vec<Finding>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Finding {
    rule_id: String,
    path: String,
    start_line: u32,
    description: String,
}

impl LintRule for Jevlint {
    fn name(&self) -> &'static str {
        "jevlint"
    }

    fn tools(&self) -> &'static [Tool] {
        &[Tool {
            probe: &["jevlint", "version"],
            version: Some(VERSION),
            install: &["cargo", "xtask", "lint", "--install", "--only", "jevlint"],
        }]
    }

    fn optional(&self) -> bool {
        true
    }

    fn required_env(&self) -> &'static [&'static str] {
        &["TYPESAFE_API_KEY"]
    }

    fn install(&self, root: &Path) -> anyhow::Result<()> {
        let tool = &self.tools()[0];
        if tool.available(root) {
            return Ok(());
        }
        let sh = Shell::new()?;
        sh.change_dir(root);
        let temporary = sh.create_temp_dir()?;
        let grammar = temporary.path().join("rust");
        let source = temporary.path().join("jevlint");
        download_module(
            &sh,
            "github.com/tree-sitter/tree-sitter-rust@v0.24.2",
            &grammar,
        )?;
        download_module(&sh, "github.com/codegirl-007/jevlint@v0.4.0", &source)?;
        let generator = if cmd!(sh, "tree-sitter --version")
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .split_whitespace()
                        .nth(1)
                        == Some("0.26.9")
            }) {
            PathBuf::from("tree-sitter")
        } else {
            let tools = temporary.path().join("tools");
            cmd!(
                sh,
                "cargo install tree-sitter-cli --version =0.26.9 --locked --root {tools}"
            )
            .run()?;
            tools.join("bin/tree-sitter")
        };
        let patch = root.join(".config/jevlint/rust.patch");
        {
            let _dir = sh.push_dir(&grammar);
            cmd!(sh, "patch -p1 -i {patch}").run()?;
            cmd!(sh, "{generator} generate").run()?;
        }
        let binary = temporary.path().join("jevlint-bin");
        {
            let _dir = sh.push_dir(&source);
            let replacement = format!(
                "-replace=github.com/tree-sitter/tree-sitter-rust={}",
                grammar.display()
            );
            cmd!(sh, "go mod edit {replacement}").run()?;
            let version =
                format!("-X github.com/codegirl-007/jevlint/internal/cli.version={VERSION}");
            cmd!(sh, "go build -a -buildvcs=false -trimpath -ldflags {version} -o {binary} ./cmd/jevlint").run()?;
        }
        check_parser(&sh, root, &binary, temporary.path())?;
        let gobin = cmd!(sh, "go env GOBIN").read()?;
        let bindir = if gobin.is_empty() {
            let gopath = cmd!(sh, "go env GOPATH").read()?;
            std::env::split_paths(&gopath)
                .next()
                .context("go env GOPATH is empty")?
                .join("bin")
        } else {
            PathBuf::from(gobin)
        };
        sh.create_dir(&bindir)?;
        let template = bindir.join(".jevlint-XXXXXXXX");
        let staged = PathBuf::from(cmd!(sh, "mktemp {template}").read()?);
        let result = (|| -> anyhow::Result<()> {
            sh.copy_file(&binary, &staged)?;
            fs::rename(&staged, bindir.join("jevlint"))?;
            Ok(())
        })();
        sh.remove_path(&staged)?;
        result?;
        tool.check(root)
    }

    fn run(&self, root: &Path) -> anyhow::Result<Vec<LintDiagnostic>> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        let output = cmd!(sh, "jevlint check --format json .")
            .env("JEVLINT_PROVIDER", "typesafe")
            .ignore_status()
            .output()?;
        ensure!(
            matches!(output.status.code(), Some(0 | 1)),
            "jevlint check failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Report =
            serde_json::from_slice(&output.stdout).context("invalid jevlint JSON report")?;
        Ok(report
            .findings
            .unwrap_or_default()
            .into_iter()
            .map(|finding| LintDiagnostic {
                severity: Severity::Error,
                file: finding.path,
                line: finding.start_line,
                end_line: finding.start_line,
                message: format!("jevlint {}: {}", finding.rule_id, finding.description),
            })
            .collect())
    }
}

fn download_module(sh: &Shell, module: &str, destination: &Path) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Module {
        #[serde(rename = "Dir")]
        directory: PathBuf,
    }
    let output = cmd!(sh, "go mod download -json {module}").read()?;
    let source = serde_json::from_str::<Module>(&output)?.directory;
    cmd!(sh, "cp -R {source} {destination}").run()?;
    cmd!(sh, "chmod -R u+w {destination}").run()?;
    Ok(())
}

fn check_parser(sh: &Shell, root: &Path, binary: &Path, temporary: &Path) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ParserReport {
        scanned_files: u32,
        code_units: u32,
        evaluations: u32,
    }
    let config = root.join(".jevlint.json");
    for (name, source, exit) in [
        (
            "macro",
            "macro_rules! empty { (~) => {}; }\nempty!(~);\n",
            0,
        ),
        ("expression", "[]\n", 0),
        ("malformed-function", "pub fn broken( {\n", 2),
        ("malformed-array", "[1,\n", 2),
    ] {
        let fixture = temporary.join(format!("{name}.rs"));
        sh.write_file(&fixture, source)?;
        let output = cmd!(
            sh,
            "{binary} check --config {config} --format json {fixture}"
        )
        .env("JEVLINT_PROVIDER", "typesafe")
        .env("TYPESAFE_API_KEY", "parser-smoke")
        .env("TYPESAFE_ENDPOINT", "http://127.0.0.1:9")
        .ignore_status()
        .output()?;
        ensure!(
            output.status.code() == Some(exit),
            "jevlint parser compatibility check `{name}` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        if exit == 0 {
            let report: ParserReport = serde_json::from_slice(&output.stdout)?;
            ensure!(report.scanned_files == 1 && report.code_units == 0 && report.evaluations == 0,
                "jevlint parser compatibility check `{name}` did not scan exactly one file without evaluations");
        } else {
            ensure!(
                String::from_utf8_lossy(&output.stderr).contains("source contains syntax errors"),
                "jevlint parser compatibility check `{name}` failed for an unrelated reason: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}
