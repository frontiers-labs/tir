use std::{collections::BTreeSet, path::Path, process::Command};

use anyhow::ensure;
use serde::Deserialize;

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
    start_line: usize,
    end_line: usize,
}

pub fn run(root: &Path) -> anyhow::Result<()> {
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(path) => std::path::PathBuf::from(path),
        None => std::path::PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?,
        )
        .join(".cache"),
    }
    .join("tir/dupehound/0.1.2");
    let binary = cache.join("bin/dupehound");
    if !binary.is_file() {
        let status = Command::new("cargo")
            .current_dir(root)
            .args([
                "install",
                "dupehound",
                "--version",
                "=0.1.2",
                "--locked",
                "--no-default-features",
                "--root",
            ])
            .arg(&cache)
            .arg("--target-dir")
            .arg(cache.join("target"))
            .status()?;
        ensure!(status.success(), "failed to install dupehound");
    }
    let output = Command::new(binary)
        .current_dir(root)
        .args([
            "scan",
            ".",
            "--json",
            "--threshold",
            "0.85",
            "--exclude",
            "**/checks/**",
            "--exclude",
            "**/.lake/**",
        ])
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
    let mut failed = false;
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
                    eprintln!(
                        "error: unaccepted duplicate-cluster members: {}:{} ({}) and {}:{} ({})",
                        left.file,
                        left.start_line,
                        left.name,
                        right.file,
                        right.start_line,
                        right.name
                    );
                    failed = true;
                }
                seen.insert(pair);
            }
        }
    }
    for pair in allowed.difference(&seen) {
        eprintln!(
            "error: {} no longer share a duplicate cluster; remove them from {ALLOWED}",
            pair.iter().cloned().collect::<Vec<_>>().join(" ")
        );
        failed = true;
    }
    failed |= crate::ast_grep::check_baseline(root, ALLOWED, &baseline)?;
    ensure!(!failed, "duplicates check failed");
    Ok(())
}
