use std::{collections::BTreeMap, path::Path, process::Command};

use anyhow::{bail, ensure, Context};
use serde::Deserialize;
use xshell::{cmd, Shell};

const AST_GREP: &str = env!("CARGO_BIN_FILE_AST_GREP_ast-grep");
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
    line: usize,
}

fn counts<'a>(lines: impl Iterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut result = BTreeMap::new();
    for line in lines {
        *result.entry(line.to_owned()).or_default() += 1;
    }
    result
}

pub fn run(root: &Path) -> anyhow::Result<()> {
    let sh = Shell::new()?;
    sh.change_dir(root);
    ensure!(
        Command::from(cmd!(
            sh,
            "{AST_GREP} test --skip-snapshot-tests -c {CONFIG}"
        ))
        .status()?
        .success(),
        "ast-grep rule tests failed"
    );
    let output = cmd!(sh, "{AST_GREP} scan --json=stream . -c {CONFIG}")
        .ignore_status()
        .output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "ast-grep failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut found = BTreeMap::<String, (usize, String)>::new();
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
        let diagnostic = format!(
            "error[{}]: {}:{}: {}",
            finding.rule_id,
            finding.file,
            finding.range.start.line + 1,
            finding.message
        );
        let entry = found.entry(key).or_insert((0, diagnostic));
        entry.0 += 1;
    }
    let baseline = std::fs::read_to_string(root.join(BASELINE))?;
    let listed = counts(baseline.lines());
    let mut failed = false;
    for (key, (count, diagnostic)) in &found {
        if *count > *listed.get(key).unwrap_or(&0) {
            eprintln!("{diagnostic}");
            failed = true;
        }
    }
    for (key, count) in &listed {
        for _ in 0..count.saturating_sub(found.get(key).map_or(0, |v| v.0)) {
            eprintln!("error: fixed, remove from {BASELINE}: {key}");
            failed = true;
        }
    }
    failed |= check_baseline(root, BASELINE, &baseline)?;
    ensure!(!failed, "ast-grep check failed");
    Ok(())
}

pub fn check_baseline(root: &Path, path: &str, baseline: &str) -> anyhow::Result<bool> {
    let mut failed = false;
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
                eprintln!("error: {path} may only shrink; new line: {key}");
                failed = true;
            }
        }
    }
    Ok(failed)
}
