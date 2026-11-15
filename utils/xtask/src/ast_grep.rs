use std::{collections::BTreeMap, path::Path};

use anyhow::{bail, ensure, Context};
use serde::Deserialize;
use xshell::{cmd, Shell};

use crate::lint::{LintDiagnostic, LintRule, Severity, Tool};

const TOOL: Tool = Tool {
    probe: &["ast-grep", "--version"],
    version: Some("0.51.0"),
    install: &[
        "cargo",
        "install",
        "ast-grep",
        "--version",
        "=0.51.0",
        "--locked",
        "--no-default-features",
    ],
};
const CONFIG: &str = ".config/ast-grep/sgconfig.yml";
const BASELINE: &str = ".config/ast-grep/baseline.tsv";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Finding {
    rule_id: String,
    file: String,
    lines: String,
    message: String,
    range: Range,
}

#[derive(Deserialize)]
struct Range {
    start: Position,
}

#[derive(Deserialize)]
struct Position {
    line: u32,
}

fn counts<'a>(lines: impl Iterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut result = BTreeMap::new();
    for line in lines {
        *result.entry(line.to_owned()).or_default() += 1;
    }
    result
}

pub struct RuleTests;
pub struct AstGrep;

impl LintRule for RuleTests {
    fn name(&self) -> &'static str {
        "ast-grep rule tests"
    }

    fn tools(&self) -> &'static [Tool] {
        &[TOOL]
    }

    fn run(&self, root: &Path) -> anyhow::Result<Vec<LintDiagnostic>> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        let output = cmd!(sh, "ast-grep test --skip-snapshot-tests -c {CONFIG}")
            .ignore_status()
            .output()?;
        ensure!(
            output.status.success(),
            "ast-grep rule tests failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(Vec::new())
    }
}

impl LintRule for AstGrep {
    fn name(&self) -> &'static str {
        "ast-grep"
    }

    fn tools(&self) -> &'static [Tool] {
        &[TOOL]
    }

    fn run(&self, root: &Path) -> anyhow::Result<Vec<LintDiagnostic>> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        let output = cmd!(sh, "ast-grep scan --json=stream . -c {CONFIG}")
            .ignore_status()
            .output()?;
        if !output.status.success() && output.status.code() != Some(1) {
            bail!(
                "ast-grep failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut found = BTreeMap::<String, Vec<LintDiagnostic>>::new();
        for line in std::str::from_utf8(&output.stdout)?.lines() {
            let finding: Finding = serde_json::from_str(line).context("invalid ast-grep output")?;
            let text = finding
                .lines
                .lines()
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let key = format!("{}\t{}\t{}", finding.rule_id, finding.file, text);
            let diagnostic = LintDiagnostic {
                severity: Severity::Error,
                file: finding.file,
                line: finding.range.start.line + 1,
                message: format!("[{}] {}", finding.rule_id, finding.message),
            };
            found.entry(key).or_default().push(diagnostic);
        }
        let baseline = std::fs::read_to_string(root.join(BASELINE))?;
        let listed = counts(baseline.lines());
        let mut errors = Vec::new();
        for (key, count) in &listed {
            for _ in 0..count.saturating_sub(found.get(key).map_or(0, Vec::len)) {
                errors.push(LintDiagnostic {
                    severity: Severity::Error,
                    file: BASELINE.to_owned(),
                    line: baseline.lines().position(|line| line == key).unwrap_or(0) as u32 + 1,
                    message: format!("fixed, remove from {BASELINE}: {key}"),
                });
            }
        }
        for (key, diagnostics) in found {
            if diagnostics.len() > *listed.get(&key).unwrap_or(&0) {
                errors.extend(diagnostics);
            }
        }
        errors.extend(check_baseline(root, BASELINE, &baseline)?);
        Ok(errors)
    }
}

pub fn check_baseline(
    root: &Path,
    path: &str,
    baseline: &str,
) -> anyhow::Result<Vec<LintDiagnostic>> {
    let mut errors = Vec::new();
    let base = std::env::var("LINT_BASE").unwrap_or_else(|_| "master".to_owned());
    let sh = Shell::new()?;
    sh.change_dir(root);
    let verify = cmd!(sh, "git rev-parse --verify --quiet {base}")
        .ignore_status()
        .output()?;
    ensure!(verify.status.success(), "cannot resolve lint base {base}");
    let revision = format!("{base}:{path}");
    let previous = cmd!(sh, "git show {revision}").ignore_status().output()?;
    if previous.status.success() {
        let mut old = String::from_utf8(previous.stdout)?;
        let renames = cmd!(
            sh,
            "git diff --name-status --find-renames --diff-filter=R {base}"
        )
        .ignore_status()
        .output()?;
        ensure!(renames.status.success(), "git diff failed");
        for line in std::str::from_utf8(&renames.stdout)?.lines() {
            let mut fields = line.split('\t');
            if let (Some(_), Some(from), Some(to)) = (fields.next(), fields.next(), fields.next()) {
                old = old.replace(from, to);
            }
        }
        let previous = counts(old.lines());
        for (key, count) in counts(baseline.lines()) {
            for _ in 0..count.saturating_sub(*previous.get(&key).unwrap_or(&0)) {
                errors.push(LintDiagnostic {
                    severity: Severity::Error,
                    file: path.to_owned(),
                    line: baseline.lines().position(|line| line == key).unwrap_or(0) as u32 + 1,
                    message: format!("{path} may only shrink; new line: {key}"),
                });
            }
        }
    }
    Ok(errors)
}
