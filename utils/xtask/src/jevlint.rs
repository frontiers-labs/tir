use std::path::Path;

use anyhow::{ensure, Context};
use serde::Deserialize;
use xshell::{cmd, Shell};

use crate::lint::{LintDiagnostic, LintRule, Severity, Tool};

pub struct Jevlint;

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
            version: Some("v0.4.0-tir.1"),
            install: &["python3", "utils/scripts/install_jevlint.py"],
        }]
    }

    fn optional(&self) -> bool {
        true
    }

    fn required_env(&self) -> &'static [&'static str] {
        &["TYPESAFE_API_KEY"]
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
