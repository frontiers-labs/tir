//! A minimal LIT-style test driver.
//!
//! This crate discovers FileCheck-style test files under a directory, extracts
//! their `RUN:` lines, executes the resulting command pipelines and reports
//! each file as an individual test case through [`libtest_mimic`]. It is meant
//! to be used from a crate's integration test with `harness = false`:
//!
//! ```ignore
//! // tests/lit.rs
//! fn main() {
//!     tir_lit::harness_main(
//!         env!("CARGO_MANIFEST_DIR"),
//!         "checks",
//!         &[("tmdlc", env!("CARGO_BIN_EXE_tmdlc"))],
//!     );
//! }
//! ```
//!
//! The driver understands a small but practical subset of LIT/lit substitution:
//! `%s` (the test file), `%S` (its directory), the `not` prefix (invert the
//! exit status) and a `| filecheck %s ...` final stage which is executed
//! in-process via the [`filecheck`] library rather than as a subprocess.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use libtest_mimic::{Arguments, Failed, Trial};

/// A discovered test together with the commands it should run.
struct TestCase {
    /// Display name, relative to the checks directory.
    name: String,
    path: PathBuf,
    run_lines: Vec<String>,
}

/// Build a workspace binary once and copy it beside the current test executable.
///
/// Running `cargo run` from every parallel lit test can race with Cargo rewriting
/// the target binary while another test tries to execute it. Tests should run
/// this stable copy instead.
pub fn cargo_test_bin(package: &str, bin: &str) -> PathBuf {
    let test_exe = std::env::current_exe().expect("current test executable path");
    let deps_dir = test_exe.parent().expect("test executable directory");
    let profile_dir = deps_dir.parent().expect("test profile directory");
    let profile = profile_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("test profile name");

    let mut cargo = Command::new("cargo");
    cargo.args(["build", "-q", "-p", package]);
    if profile == "release" {
        cargo.arg("--release");
    }
    let status = cargo.status().expect("spawn cargo build");
    assert!(
        status.success(),
        "cargo build -p {package} failed: {status}"
    );

    let source = profile_dir.join(bin.to_owned() + std::env::consts::EXE_SUFFIX);
    let dest = deps_dir.join(format!(
        "{}-lit-{}{}",
        bin,
        std::process::id(),
        std::env::consts::EXE_SUFFIX
    ));
    std::fs::copy(&source, &dest).unwrap_or_else(|e| {
        panic!(
            "copy built binary from '{}' to '{}': {e}",
            source.display(),
            dest.display()
        )
    });
    dest
}

/// Collect tests, run them through libtest-mimic and exit the process.
///
/// `manifest_dir` is typically `env!("CARGO_MANIFEST_DIR")`, `checks_subdir`
/// the directory (relative to it) that holds the tests, and `tools` a mapping
/// from the tool name used in `RUN:` lines to its built executable path.
pub fn harness_main(manifest_dir: &str, checks_subdir: &str, tools: &[(&str, &str)]) {
    let args = Arguments::from_args();
    let checks_dir = Path::new(manifest_dir).join(checks_subdir);

    let tool_map: HashMap<String, PathBuf> = tools
        .iter()
        .map(|(k, v)| (k.to_string(), PathBuf::from(v)))
        .collect();

    let cases = match discover(&checks_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tir-lit: failed to discover tests in {checks_dir:?}: {e}");
            std::process::exit(2);
        }
    };

    let trials = cases
        .into_iter()
        .map(|case| {
            let tool_map = tool_map.clone();
            Trial::test(case.name.clone(), move || run_case(&case, &tool_map))
        })
        .collect();

    libtest_mimic::run(&args, trials).exit();
}

/// Recursively find test files that contain at least one `RUN:` line.
fn discover(dir: &Path) -> std::io::Result<Vec<TestCase>> {
    let mut cases = Vec::new();
    visit(dir, dir, &mut cases)?;
    cases.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cases)
}

fn visit(root: &Path, dir: &Path, cases: &mut Vec<TestCase>) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // `Inputs` directories hold fixtures, not tests.
            if path.file_name() == Some(OsStr::new("Inputs")) {
                continue;
            }
            visit(root, &path, cases)?;
        } else if file_type.is_file() {
            if let Some(case) = load_case(root, &path)? {
                cases.push(case);
            }
        }
    }
    Ok(())
}

fn load_case(root: &Path, path: &Path) -> std::io::Result<Option<TestCase>> {
    let contents = std::fs::read_to_string(path)?;
    let run_lines = extract_run_lines(&contents);
    if run_lines.is_empty() {
        return Ok(None);
    }
    let name = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    Ok(Some(TestCase {
        name,
        path: path.to_path_buf(),
        run_lines,
    }))
}

/// Extract `RUN:` command text from a test file, joining lines continued with a
/// trailing backslash.
fn extract_run_lines(contents: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut continued: Option<String> = None;

    for raw in contents.lines() {
        if let Some(prev) = continued.take() {
            let trimmed = raw.trim();
            let body = trimmed.trim_start_matches("//").trim();
            if let Some(stripped) = body.strip_suffix('\\') {
                continued = Some(format!("{prev} {}", stripped.trim()));
            } else {
                lines.push(format!("{prev} {body}"));
            }
            continue;
        }

        if let Some(idx) = raw.find("RUN:") {
            let cmd = raw[idx + "RUN:".len()..].trim();
            if let Some(stripped) = cmd.strip_suffix('\\') {
                continued = Some(stripped.trim().to_string());
            } else {
                lines.push(cmd.to_string());
            }
        }
    }

    if let Some(prev) = continued {
        lines.push(prev);
    }
    lines
}

fn run_case(case: &TestCase, tools: &HashMap<String, PathBuf>) -> Result<(), Failed> {
    for run in &case.run_lines {
        run_pipeline(run, &case.path, tools)?;
    }
    Ok(())
}

/// Execute a single `RUN:` pipeline.
fn run_pipeline(
    run: &str,
    test_path: &Path,
    tools: &HashMap<String, PathBuf>,
) -> Result<(), Failed> {
    let s = test_path.to_string_lossy().to_string();
    let dir = test_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let substitute = |tok: &str| tok.replace("%s", &s).replace("%S", &dir);

    let stages: Vec<&str> = run.split('|').collect();
    let mut piped_input: Vec<u8> = Vec::new();

    for (idx, stage) in stages.iter().enumerate() {
        let mut tokens: Vec<String> = stage.split_whitespace().map(substitute).collect();
        if tokens.is_empty() {
            return Err(Failed::from(format!(
                "empty pipeline stage in `RUN: {run}`"
            )));
        }

        let mut invert = false;
        if tokens[0] == "not" {
            invert = true;
            tokens.remove(0);
        }
        if tokens.is_empty() {
            return Err(Failed::from(format!(
                "`not` with no command in `RUN: {run}`"
            )));
        }

        let program = tokens.remove(0);
        let is_last = idx + 1 == stages.len();

        if program == "filecheck" {
            // Final, in-process FileCheck stage.
            let result = run_filecheck(&tokens, &piped_input);
            let ok = result.is_ok();
            if invert {
                if ok {
                    return Err(Failed::from(format!(
                        "expected `not filecheck` to fail, but it succeeded\nRUN: {run}"
                    )));
                }
            } else if let Err(diag) = result {
                return Err(Failed::from(format!("RUN: {run}\n\n{diag}")));
            }
            piped_input.clear();
        } else {
            let resolved = tools
                .get(&program)
                .cloned()
                .unwrap_or_else(|| PathBuf::from(&program));
            let output = run_subprocess(&resolved, &tokens, &piped_input).map_err(|e| {
                Failed::from(format!("failed to spawn `{program}`: {e}\nRUN: {run}"))
            })?;

            let success = output.status.success();
            if invert == success {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let verb = if invert { "succeed" } else { "fail" };
                return Err(Failed::from(format!(
                    "command `{program}` was not expected to {verb} (exit: {})\nRUN: {run}\n--- stderr ---\n{stderr}",
                    output.status,
                )));
            }
            piped_input = output.stdout;

            if is_last && !piped_input.is_empty() {
                // Nothing consumes the output; that's fine.
            }
        }
    }

    Ok(())
}

fn run_subprocess(
    program: &Path,
    args: &[String],
    input: &[u8],
) -> std::io::Result<std::process::Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    child.wait_with_output()
}

/// Run the in-process FileCheck stage.
fn run_filecheck(args: &[String], input: &[u8]) -> Result<(), String> {
    let (check_path, config) = parse_filecheck_args(args)?;
    let check_text = std::fs::read_to_string(&check_path)
        .map_err(|e| format!("cannot read check file '{}': {e}", check_path.display()))?;
    let input_text = String::from_utf8_lossy(input).into_owned();

    let check = filecheck::Source::new(check_path.display().to_string(), check_text);
    let input = filecheck::Source::new("<stdout>", input_text);
    filecheck::verify(&check, &input, &config)
}

/// Parse the arguments of a `filecheck` stage into a check-file path and a
/// [`filecheck::Config`].
fn parse_filecheck_args(args: &[String]) -> Result<(PathBuf, filecheck::Config), String> {
    let mut config = filecheck::Config::default();
    let mut check_path: Option<PathBuf> = None;
    let mut it = args.iter();

    while let Some(arg) = it.next() {
        let take_value =
            |it: &mut std::slice::Iter<String>, inline: Option<&str>| -> Option<String> {
                inline.map(|s| s.to_string()).or_else(|| it.next().cloned())
            };

        if let Some(rest) = arg.strip_prefix("--check-prefix=") {
            config.check_prefixes.extend(split_csv(rest));
        } else if let Some(rest) = arg.strip_prefix("--check-prefixes=") {
            config.check_prefixes.extend(split_csv(rest));
        } else if arg == "--check-prefix" || arg == "--check-prefixes" {
            if let Some(v) = take_value(&mut it, None) {
                config.check_prefixes.extend(split_csv(&v));
            }
        } else if let Some(rest) = arg.strip_prefix("--comment-prefixes=") {
            config.comment_prefixes.extend(split_csv(rest));
        } else if arg == "--comment-prefixes" {
            if let Some(v) = take_value(&mut it, None) {
                config.comment_prefixes.extend(split_csv(&v));
            }
        } else if let Some(rest) = arg.strip_prefix("--implicit-check-not=") {
            config.implicit_check_not.push(rest.to_string());
        } else if arg == "--implicit-check-not" {
            if let Some(v) = take_value(&mut it, None) {
                config.implicit_check_not.push(v);
            }
        } else if arg == "--strict-whitespace" {
            config.strict_whitespace = true;
        } else if arg == "--match-full-lines" {
            config.match_full_lines = true;
        } else if arg == "--allow-empty" {
            config.allow_empty = true;
        } else if arg.starts_with('-') && arg != "-" {
            return Err(format!("unknown filecheck argument: {arg}"));
        } else if check_path.is_none() {
            check_path = Some(PathBuf::from(arg));
        }
    }

    let check_path = check_path.ok_or("filecheck: no check file specified")?;
    Ok((check_path, config))
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}
