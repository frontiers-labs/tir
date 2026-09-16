use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{ensure, Context};
use clap::Args;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Args)]
pub struct Options {
    /// Milestone manifest to run, for example m01.
    milestone: String,
    /// Fail when a required case is missing, skipped, or unsupported.
    #[arg(long)]
    strict: bool,
    /// Override the repository milestone manifest.
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// Override the JSON report path.
    #[arg(long)]
    report: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    milestone: String,
    #[serde(default)]
    fixtures: Vec<PathBuf>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    required: bool,
    command: Vec<String>,
    tool: String,
    expected_cases: u64,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    milestone: String,
    strict: bool,
    delivered: bool,
    revision: String,
    workspace_state_sha256: String,
    runner_sha256: String,
    simulator_sha256: Option<String>,
    manifest_sha256: String,
    fixture_sha256: BTreeMap<String, String>,
    tool_versions: BTreeMap<String, String>,
    summary: Summary,
    results: Vec<CaseResult>,
}

#[derive(Default, Serialize)]
struct Summary {
    passed: u64,
    failed: u64,
    skipped: u64,
    unsupported: u64,
}

#[derive(Serialize)]
struct CaseResult {
    name: String,
    required: bool,
    status: Status,
    cases_passed: u64,
    cases_failed: u64,
    cases_ignored: u64,
    test_names: Vec<String>,
    output_artifact: String,
    output_sha256: String,
    detail: String,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pass,
    Fail,
    Skipped,
    Unsupported,
}

pub fn run(root: &Path, options: Options) -> anyhow::Result<()> {
    let manifest_path = options.manifest.unwrap_or_else(|| {
        root.join("xtask/isasim-accept")
            .join(format!("{}.toml", options.milestone))
    });
    let manifest_bytes = fs::read(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: Manifest = toml::from_slice(&manifest_bytes)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    ensure!(
        manifest.schema_version == 1,
        "unsupported acceptance manifest schema"
    );
    ensure!(
        manifest.milestone == options.milestone,
        "manifest milestone does not match command"
    );

    let report_path = options.report.unwrap_or_else(|| {
        root.join("target/isasim-accept")
            .join(&options.milestone)
            .join("report.json")
    });
    let report_directory = report_path.parent().context("report path has no parent")?;
    fs::create_dir_all(report_directory)?;
    fs::write(report_directory.join("manifest.toml"), &manifest_bytes)?;

    let fixture_sha256 = hash_fixtures(root, &manifest.fixtures)?;
    let revision =
        command_line(root, "git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let mut tool_versions = BTreeMap::new();
    tool_versions.insert(
        "rustc".into(),
        command_line(root, "rustc", &["--version"]).unwrap_or_else(|| "unavailable".into()),
    );
    let mut summary = Summary::default();
    let mut results = Vec::new();
    for case in &manifest.cases {
        tool_versions.entry(case.tool.clone()).or_insert_with(|| {
            command_line(root, &case.tool, &["--version"]).unwrap_or_else(|| "unavailable".into())
        });
        let result = run_case(root, report_directory, case)?;
        match result.status {
            Status::Pass => summary.passed += 1,
            Status::Fail => summary.failed += 1,
            Status::Skipped => summary.skipped += 1,
            Status::Unsupported => summary.unsupported += 1,
        }
        results.push(result);
    }

    let has_required = manifest.cases.iter().any(|case| case.required);
    let required_incomplete = results
        .iter()
        .any(|result| result.required && !matches!(result.status, Status::Pass));
    let delivered = options.strict
        && has_required
        && !required_incomplete
        && summary.failed == 0
        && summary.unsupported == 0
        && summary.skipped == 0;
    let runner = std::env::current_exe()?;
    let simulator = runner
        .parent()
        .context("runner path has no parent")?
        .join("tir-isasim");
    let report = Report {
        schema_version: 1,
        milestone: manifest.milestone,
        strict: options.strict,
        delivered,
        revision,
        workspace_state_sha256: workspace_state_sha256(root)?,
        runner_sha256: sha256(&fs::read(runner)?),
        simulator_sha256: fs::read(simulator).ok().map(|bytes| sha256(&bytes)),
        manifest_sha256: sha256(&manifest_bytes),
        fixture_sha256,
        tool_versions,
        summary,
        results,
    };
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("acceptance report: {}", report_path.display());

    ensure!(
        report.summary.failed == 0,
        "one or more acceptance cases failed"
    );
    ensure!(
        !options.strict || report.delivered,
        "strict acceptance requires every required case to pass"
    );
    Ok(())
}

fn run_case(root: &Path, report_directory: &Path, case: &Case) -> anyhow::Result<CaseResult> {
    if case.command.is_empty() {
        return Ok(case_result(
            case,
            Status::Unsupported,
            (0, 0, 0),
            &[],
            "empty command",
            "",
        ));
    }
    let output = match Command::new(&case.command[0])
        .args(&case.command[1..])
        .env_remove("LIT_FILTER")
        .env_remove("LIT_FILTER_OUT")
        .envs(&case.env)
        .current_dir(root)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(case_result(
                case,
                Status::Unsupported,
                (0, 0, 0),
                &[],
                "tool unavailable",
                "",
            ));
        }
        Err(error) => {
            return Ok(case_result(
                case,
                Status::Fail,
                (0, 1, 0),
                &[],
                &error.to_string(),
                "",
            ));
        }
    };
    let mut bytes = output.stdout;
    bytes.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&bytes);
    let (passed, failed, ignored) = test_counts(&text);
    let artifact_name = format!("{}.log", safe_name(&case.name));
    fs::write(report_directory.join(&artifact_name), &bytes)?;
    let (status, detail) = if !output.status.success() || failed != 0 {
        (
            Status::Fail,
            format!("command exited with {}", output.status),
        )
    } else if passed != case.expected_cases || ignored != 0 {
        (
            Status::Skipped,
            format!(
                "observed {passed} passed and {ignored} ignored; expected exactly {} passed and none ignored",
                case.expected_cases
            ),
        )
    } else {
        (Status::Pass, String::new())
    };
    Ok(case_result(
        case,
        status,
        (passed, failed, ignored),
        &bytes,
        &detail,
        &artifact_name,
    ))
}

fn case_result(
    case: &Case,
    status: Status,
    counts: (u64, u64, u64),
    output: &[u8],
    detail: &str,
    artifact: &str,
) -> CaseResult {
    let (passed, failed, ignored) = counts;
    CaseResult {
        name: case.name.clone(),
        required: case.required,
        status,
        cases_passed: passed,
        cases_failed: failed,
        cases_ignored: ignored,
        test_names: test_names(&String::from_utf8_lossy(output)),
        output_artifact: artifact.into(),
        output_sha256: sha256(output),
        detail: detail.into(),
    }
}

fn test_names(output: &str) -> Vec<String> {
    let pattern = Regex::new(r"(?m)^test (\S+)\s+\.\.\. (?:ok|FAILED|ignored)$").unwrap();
    pattern
        .captures_iter(output)
        .map(|captures| captures[1].to_string())
        .collect()
}

fn test_counts(output: &str) -> (u64, u64, u64) {
    let pattern =
        Regex::new(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;")
            .unwrap();
    pattern
        .captures_iter(output)
        .fold((0, 0, 0), |totals, captures| {
            (
                totals.0 + captures[1].parse::<u64>().unwrap(),
                totals.1 + captures[2].parse::<u64>().unwrap(),
                totals.2 + captures[3].parse::<u64>().unwrap(),
            )
        })
}

fn hash_fixtures(root: &Path, fixtures: &[PathBuf]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut files = Vec::new();
    for fixture in fixtures {
        collect_fixture_files(root, fixture, &mut files)?;
    }
    files.sort();
    files.dedup();
    files
        .into_iter()
        .map(|fixture| {
            let bytes = fs::read(root.join(&fixture))?;
            Ok((fixture.display().to_string(), sha256(&bytes)))
        })
        .collect()
}

fn collect_fixture_files(
    root: &Path,
    fixture: &Path,
    files: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let path = root.join(fixture);
    ensure!(path.exists(), "missing fixture {}", fixture.display());
    if path.is_file() {
        files.push(fixture.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let relative = fixture.join(entry.file_name());
        collect_fixture_files(root, &relative, files)?;
    }
    Ok(())
}

fn workspace_state_sha256(root: &Path) -> anyhow::Result<String> {
    let diff = Command::new("git")
        .args(["diff", "--binary", "HEAD"])
        .current_dir(root)
        .output()?;
    ensure!(diff.status.success(), "git diff failed");
    let status = Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .current_dir(root)
        .output()?;
    ensure!(status.status.success(), "git status failed");
    let mut state = diff.stdout;
    state.extend_from_slice(&status.stdout);
    for line in String::from_utf8_lossy(&status.stdout).lines() {
        if let Some(path) = line.strip_prefix("?? ") {
            state.extend_from_slice(&fs::read(root.join(path))?);
        }
    }
    Ok(sha256(&state))
}

fn safe_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn command_line(root: &Path, program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.input(bytes);
    format!("sha256:{:x}", digest.result())
}
