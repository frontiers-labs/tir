use std::{collections::BTreeSet, path::Path};

use anyhow::ensure;
use serde::Deserialize;
use xshell::{cmd, Shell};

use crate::lint::{LintDiagnostic, LintRule, Severity, Tool};

const ALLOWED: &str = ".config/dupehound.allow";

#[derive(Deserialize)]
struct Report {
    schema_version: u32,
    clusters: Vec<Cluster>,
}

#[derive(Deserialize)]
struct Cluster {
    members: Vec<Member>,
}

#[derive(Deserialize)]
struct Member {
    file: String,
    name: String,
    start_line: u32,
    end_line: u32,
}

pub struct Dupehound;

impl LintRule for Dupehound {
    fn name(&self) -> &'static str {
        "duplicate code"
    }

    fn tools(&self) -> &'static [Tool] {
        &[Tool {
            probe: &["dupehound", "--version"],
            version: Some("0.1.2"),
            install: &[
                "cargo",
                "install",
                "dupehound",
                "--version",
                "=0.1.2",
                "--locked",
                "--no-default-features",
            ],
        }]
    }

    fn run(&self, root: &Path) -> anyhow::Result<Vec<LintDiagnostic>> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        let output = cmd!(
            sh,
            "dupehound scan . --json --threshold 0.85 --exclude **/checks/** --exclude **/.lake/**"
        )
        .ignore_status()
        .output()?;
        ensure!(
            output.status.success(),
            "dupehound failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Report = serde_json::from_slice(&output.stdout)?;
        ensure!(
            report.schema_version == 1,
            "unsupported dupehound JSON schema: {}",
            report.schema_version
        );
        let baseline = std::fs::read_to_string(root.join(ALLOWED))?;
        let allowed: BTreeSet<BTreeSet<String>> = baseline
            .lines()
            .filter_map(|line| {
                let pair: BTreeSet<_> = line
                    .split('#')
                    .next()
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect();
                (!pair.is_empty()).then_some(pair)
            })
            .collect();
        let mut seen = BTreeSet::new();
        let mut errors = Vec::new();
        for cluster in report.clusters {
            let members: Vec<_> = cluster
                .members
                .iter()
                .filter(|m| m.end_line.saturating_sub(m.start_line) + 1 >= 11)
                .collect();
            for (index, left) in members.iter().enumerate() {
                for right in &members[index + 1..] {
                    let pair = BTreeSet::from([
                        format!("{}::{}", left.file, left.name),
                        format!("{}::{}", right.file, right.name),
                    ]);
                    if !allowed.contains(&pair) {
                        errors.push(LintDiagnostic {
                            severity: Severity::Error,
                            file: left.file.clone(),
                            line: left.start_line,
                            message: format!(
                                "unaccepted duplicate-cluster members: {}:{} ({}) and {}:{} ({})",
                                left.file,
                                left.start_line,
                                left.name,
                                right.file,
                                right.start_line,
                                right.name
                            ),
                        });
                    }
                    seen.insert(pair);
                }
            }
        }
        for pair in allowed.difference(&seen) {
            errors.push(LintDiagnostic {
                severity: Severity::Error,
                file: ALLOWED.to_owned(),
                line: baseline
                    .lines()
                    .position(|line| {
                        let members: BTreeSet<_> = line
                            .split('#')
                            .next()
                            .unwrap_or_default()
                            .split_whitespace()
                            .map(str::to_owned)
                            .collect();
                        &members == pair
                    })
                    .unwrap_or(0) as u32
                    + 1,
                message: format!(
                    "{} no longer share a duplicate cluster; remove them from {ALLOWED}",
                    pair.iter().cloned().collect::<Vec<_>>().join(" ")
                ),
            });
        }
        errors.extend(crate::ast_grep::check_baseline(root, ALLOWED, &baseline)?);
        Ok(errors)
    }
}
