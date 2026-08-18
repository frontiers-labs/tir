//! A minimal LIT-style test driver.
//!
//! [`workspace_harness_main`] walks up from `CARGO_MANIFEST_DIR` to the
//! workspace root, then discovers every test suite below it. A suite is any
//! directory holding a `test_suite.toml`:
//!
//! ```toml
//! [suite]
//! name = "TIR RISC-V backend checks"
//! glob = ["**/*.S", "**/*.tir", "!**/Inputs/**/*"]
//! ```
//!
//! The `glob` patterns are relative to the suite directory and select its test
//! files; a leading `!` excludes. The driver extracts the `RUN:` lines of each
//! selected file, executes the resulting command pipelines and reports the file
//! as an individual test case through [`libtest_mimic`], named by its path
//! relative to the workspace root (e.g. `backends/riscv/checks/GISel/add.tir`).
//! It is meant to be used from a single integration test with `harness = false`:
//!
//! ```ignore
//! // tests/lit.rs
//! fn main() {
//!     tir_lit::workspace_harness_main(&[
//!         ("tmdlc", tir_lit::Tool::cargo_test_bin("tmdl", "tmdlc")),
//!     ]);
//! }
//! ```
//!
//! Following LLVM LIT, the `LIT_FILTER` environment variable (a regex) runs
//! only the tests whose full name matches it, and `LIT_FILTER_OUT` excludes
//! matching tests. An invalid regex aborts with exit status 2.
//!
//! The driver understands a small but practical subset of LIT/lit substitution:
//! `%s` (the test file), `%S` (its directory), the `not` prefix (invert the
//! exit status) and a `| filecheck %s ...` final stage which is executed
//! in-process via the [`filecheck`] library rather than as a subprocess. It also
//! supports unconditional `XFAIL: *` expected failures.
//!
//! Tests can be gated on host platform features with `REQUIRES:` and
//! `UNSUPPORTED:` lines, each a comma-separated list of feature tokens (a
//! leading `!` negates a token). A test runs only if every `REQUIRES` token is
//! available and no `UNSUPPORTED` token is available; multiple directive lines
//! in one file combine (all `REQUIRES` lines must pass, any `UNSUPPORTED` line
//! can skip). Gated-out tests are reported as ignored, not passed or failed.
//! The available feature tokens are the host OS (`linux`, `macos`, `windows`)
//! and architecture (`x86_64`, `riscv64`, or `aarch64`/`arm64` as aliases on
//! ARM64).
//!
//! Both `//` and `#` comment styles are recognized for all directives.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use globset::{Glob, GlobSet, GlobSetBuilder};
use libtest_mimic::{Arguments, Failed, Trial};
use regex::Regex;
use serde::Deserialize;

/// Whether a test with the given display name should run under the
/// `LIT_FILTER`/`LIT_FILTER_OUT` regexes.
fn filter_decision(name: &str, filter: Option<&Regex>, filter_out: Option<&Regex>) -> bool {
    filter.is_none_or(|re| re.is_match(name)) && !filter_out.is_some_and(|re| re.is_match(name))
}

/// A discovered test together with the commands it should run.
struct TestCase {
    /// Display name, relative to the checks directory.
    name: String,
    path: PathBuf,
    run_lines: Vec<String>,
    xfail: bool,
    ignored: bool,
}

/// Whether a test should run or be skipped as unavailable on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Run,
    Ignore,
}

/// Compute the set of platform feature tokens available on this host.
fn host_features() -> HashSet<String> {
    let mut features = HashSet::new();
    features.insert(std::env::consts::OS.to_string());
    match std::env::consts::ARCH {
        "aarch64" => {
            features.insert("aarch64".to_string());
            features.insert("arm64".to_string());
        }
        other => {
            features.insert(other.to_string());
        }
    }
    features
}

/// Whether a (possibly `!`-negated) feature token is satisfied by `features`.
fn token_satisfied(features: &HashSet<String>, token: &str) -> bool {
    match token.strip_prefix('!') {
        Some(negated) => !features.contains(negated),
        None => features.contains(token),
    }
}

/// Collect the comma-separated feature tokens of every line containing
/// `directive` (e.g. `"REQUIRES:"`), regardless of comment prefix.
fn collect_directive_tokens(contents: &str, directive: &str) -> Vec<String> {
    contents
        .lines()
        .filter_map(|line| {
            line.find(directive)
                .map(|idx| &line[idx + directive.len()..])
        })
        .flat_map(split_csv)
        .collect()
}

/// Decide whether a test file should run on a host with the given features.
fn gating_decision(features: &HashSet<String>, contents: &str) -> Gate {
    let requires = collect_directive_tokens(contents, "REQUIRES:");
    if !requires.iter().all(|t| token_satisfied(features, t)) {
        return Gate::Ignore;
    }

    let unsupported = collect_directive_tokens(contents, "UNSUPPORTED:");
    if unsupported.iter().any(|t| token_satisfied(features, t)) {
        return Gate::Ignore;
    }

    Gate::Run
}

/// Build a workspace binary and snapshot it beside the current test executable
/// under a stable name.
///
/// Lit tests must not execute `target/<profile>/<bin>` directly: concurrent
/// test processes (e.g. under `cargo nextest run`, which re-enters `main` for
/// every test) may run `cargo build` while another process execs the binary,
/// and Cargo rewrites it in place. The snapshot is repointed with an atomic
/// rename instead, so an exec always sees a complete binary — old or new,
/// never partially written — and stale copies do not accumulate.
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

    let suffix = std::env::consts::EXE_SUFFIX;
    let source = profile_dir.join(bin.to_owned() + suffix);
    let dest = deps_dir.join(format!("{bin}-lit{suffix}"));
    let tmp = deps_dir.join(format!("{bin}-lit-{}{suffix}", std::process::id()));

    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(&source, &tmp).is_err() {
        std::fs::copy(&source, &tmp).unwrap_or_else(|e| {
            panic!(
                "copy built binary from '{}' to '{}': {e}",
                source.display(),
                tmp.display()
            )
        });
    }
    match std::fs::rename(&tmp, &dest) {
        Ok(()) => {
            // POSIX defines rename between two hard links of the same inode
            // as a no-op that succeeds and leaves `tmp` in place.
            let _ = std::fs::remove_file(&tmp);
            dest
        }
        // Windows cannot replace an executable that is currently running; the
        // process-unique snapshot still works there.
        Err(_) => tmp,
    }
}

#[derive(Clone)]
pub enum Tool {
    Path(PathBuf),
    CargoTestBin {
        package: &'static str,
        bin: &'static str,
        path: Arc<OnceLock<PathBuf>>,
    },
}

impl Tool {
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into())
    }

    pub fn cargo_test_bin(package: &'static str, bin: &'static str) -> Self {
        Self::CargoTestBin {
            package,
            bin,
            path: Arc::new(OnceLock::new()),
        }
    }

    fn resolve(&self) -> PathBuf {
        match self {
            Self::Path(path) => path.clone(),
            Self::CargoTestBin { package, bin, path } => {
                path.get_or_init(|| cargo_test_bin(package, bin)).clone()
            }
        }
    }
}

/// Walk up from `start` to the first directory whose `Cargo.toml` contains a
/// `[workspace]` section.
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
        manifest.contains("[workspace]").then(|| dir.to_path_buf())
    })
}

/// A test suite declared by a `test_suite.toml` file.
struct Suite {
    /// Directory holding the `test_suite.toml`.
    root: PathBuf,
    /// Human-readable suite name, used in diagnostics.
    name: String,
    include: GlobSet,
    exclude: GlobSet,
}

#[derive(Deserialize)]
struct SuiteFile {
    suite: SuiteConfig,
}

#[derive(Deserialize)]
struct SuiteConfig {
    name: String,
    /// Glob patterns relative to the suite root; a leading `!` excludes.
    glob: Vec<String>,
}

const SUITE_FILE: &str = "test_suite.toml";

/// Parse a `test_suite.toml`, splitting its globs into include and exclude sets.
fn load_suite(manifest: &Path) -> Result<Suite, String> {
    let contents = std::fs::read_to_string(manifest).map_err(|e| e.to_string())?;
    let parsed: SuiteFile = toml::from_str(&contents).map_err(|e| e.to_string())?;
    let mut include = GlobSetBuilder::new();
    let mut exclude = GlobSetBuilder::new();
    for pattern in &parsed.suite.glob {
        let (builder, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (&mut exclude, rest),
            None => (&mut include, pattern.as_str()),
        };
        builder.add(Glob::new(pattern).map_err(|e| e.to_string())?);
    }
    Ok(Suite {
        root: manifest.parent().unwrap_or(Path::new(".")).to_path_buf(),
        name: parsed.suite.name,
        include: include.build().map_err(|e| e.to_string())?,
        exclude: exclude.build().map_err(|e| e.to_string())?,
    })
}

/// Recursively find every `test_suite.toml` below `root`, sorted by path.
/// Skips `target`, `node_modules` and hidden directories.
fn discover_suites(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut manifests = Vec::new();
    visit_suites(root, &mut manifests)?;
    manifests.sort();
    Ok(manifests)
}

fn visit_suites(dir: &Path, manifests: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let manifest = dir.join(SUITE_FILE);
    if manifest.is_file() {
        manifests.push(manifest);
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        visit_suites(&entry.path(), manifests)?;
    }
    Ok(())
}

/// Read an environment variable as a regex, exiting with status 2 on an
/// invalid pattern.
fn env_regex(var: &str) -> Option<Regex> {
    let pattern = std::env::var(var).ok()?;
    match Regex::new(&pattern) {
        Ok(re) => Some(re),
        Err(e) => {
            eprintln!("tir-lit: invalid {var} regex: {e}");
            std::process::exit(2);
        }
    }
}

/// Collect every check in the workspace, run the tests through libtest-mimic
/// and exit the process.
///
/// The workspace root is found by walking up from `CARGO_MANIFEST_DIR`;
/// every `checks` directory with a sibling `Cargo.toml` below it contributes
/// tests named `<crate dir>/<path under checks>`. `tools` maps the tool names
/// used in `RUN:` lines to executables. `LIT_FILTER` and `LIT_FILTER_OUT`
/// restrict which tests run (see the crate docs).
pub fn workspace_harness_main(tools: &[(&str, Tool)]) {
    let args = Arguments::from_args();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let root = find_workspace_root(Path::new(&manifest_dir)).unwrap_or_else(|| {
        eprintln!("tir-lit: no [workspace] Cargo.toml above {manifest_dir}");
        std::process::exit(2);
    });
    let manifests = discover_suites(&root).unwrap_or_else(|e| {
        eprintln!("tir-lit: failed to discover {SUITE_FILE} files in {root:?}: {e}");
        std::process::exit(2);
    });

    let filter = env_regex("LIT_FILTER");
    let filter_out = env_regex("LIT_FILTER_OUT");

    let tool_map: HashMap<String, Tool> = tools
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();

    let features = host_features();
    let mut trials = Vec::new();
    for manifest in manifests {
        let suite = load_suite(&manifest).unwrap_or_else(|e| {
            eprintln!("tir-lit: invalid {}: {e}", manifest.display());
            std::process::exit(2);
        });
        let cases = match discover(&suite, &root, &features) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("tir-lit: failed to discover tests of {}: {e}", suite.name);
                std::process::exit(2);
            }
        };
        for case in cases {
            if !filter_decision(&case.name, filter.as_ref(), filter_out.as_ref()) {
                continue;
            }
            let tool_map = tool_map.clone();
            let ignored = case.ignored;
            trials.push(
                Trial::test(case.name.clone(), move || run_case(&case, &tool_map))
                    .with_ignored_flag(ignored),
            );
        }
    }

    libtest_mimic::run(&args, trials).exit();
}

impl Suite {
    /// Whether a path relative to the suite root is one of its tests.
    fn matches(&self, relative: &Path) -> bool {
        self.include.is_match(relative) && !self.exclude.is_match(relative)
    }
}

/// Collect the suite's test files: those matching its globs that hold at least
/// one `RUN:` line. Test names are relative to `workspace_root`.
fn discover(
    suite: &Suite,
    workspace_root: &Path,
    features: &HashSet<String>,
) -> std::io::Result<Vec<TestCase>> {
    let mut cases = Vec::new();
    visit(suite, workspace_root, &suite.root, features, &mut cases)?;
    cases.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cases)
}

fn visit(
    suite: &Suite,
    workspace_root: &Path,
    dir: &Path,
    features: &HashSet<String>,
    cases: &mut Vec<TestCase>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            visit(suite, workspace_root, &path, features, cases)?;
        } else if file_type.is_file() && suite.matches(relative(&path, &suite.root)) {
            let name = relative(&path, workspace_root)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(case) = load_case(name, &path, features)? {
                cases.push(case);
            }
        }
    }
    Ok(())
}

fn relative<'a>(path: &'a Path, base: &Path) -> &'a Path {
    path.strip_prefix(base).unwrap_or(path)
}

fn load_case(
    name: String,
    path: &Path,
    features: &HashSet<String>,
) -> std::io::Result<Option<TestCase>> {
    let contents = std::fs::read_to_string(path)?;
    let run_lines = extract_run_lines(&contents);
    if run_lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(TestCase {
        name,
        path: path.to_path_buf(),
        run_lines,
        xfail: has_unconditional_xfail(&contents),
        ignored: gating_decision(features, &contents) == Gate::Ignore,
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

fn has_unconditional_xfail(contents: &str) -> bool {
    contents.lines().any(|line| {
        line.find("XFAIL:").is_some_and(|idx| {
            line[idx + "XFAIL:".len()..]
                .split(',')
                .any(|item| item.trim() == "*")
        })
    })
}

fn run_case(case: &TestCase, tools: &HashMap<String, Tool>) -> Result<(), Failed> {
    let result = run_case_commands(case, tools);
    if !case.xfail {
        return result;
    }

    match result {
        Ok(()) => Err(Failed::from(format!("XPASS: {}", case.name))),
        Err(_) => Ok(()),
    }
}

fn run_case_commands(case: &TestCase, tools: &HashMap<String, Tool>) -> Result<(), Failed> {
    for run in &case.run_lines {
        run_pipeline(run, &case.path, tools)?;
    }
    Ok(())
}

/// Execute a single `RUN:` pipeline.
fn run_pipeline(run: &str, test_path: &Path, tools: &HashMap<String, Tool>) -> Result<(), Failed> {
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

        let merge_stderr = tokens.iter().any(|token| token == "2>&1");
        tokens.retain(|token| token != "2>&1");

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

        // `env NAME=VALUE ... cmd` is handled here rather than by exec'ing
        // /usr/bin/env, so that `cmd` still goes through tool-name resolution.
        let mut envs: Vec<(String, String)> = Vec::new();
        if tokens[0] == "env" {
            tokens.remove(0);
            while !tokens.is_empty() {
                let Some((name, value)) = tokens[0].split_once('=') else {
                    break;
                };
                let assignment = (name.to_string(), value.to_string());
                tokens.remove(0);
                envs.push(assignment);
            }
            if tokens.is_empty() {
                return Err(Failed::from(format!(
                    "`env` with no command in `RUN: {run}`"
                )));
            }
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
                .map(Tool::resolve)
                .unwrap_or_else(|| PathBuf::from(&program));
            let output = run_subprocess(&resolved, &tokens, &envs, &piped_input).map_err(|e| {
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
            if merge_stderr {
                piped_input.extend_from_slice(&output.stderr);
            }

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
    envs: &[(String, String)],
    input: &[u8],
) -> std::io::Result<std::process::Output> {
    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn re(pattern: &str) -> Regex {
        Regex::new(pattern).unwrap()
    }

    fn suite(globs: &str) -> (tempfile::TempDir, Suite) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(SUITE_FILE),
            format!("[suite]\nname = \"demo\"\nglob = {globs}\n"),
        )
        .unwrap();
        let suite = load_suite(&tmp.path().join(SUITE_FILE)).unwrap();
        (tmp, suite)
    }

    #[test]
    fn suite_globs_select_tests_and_exclusions_win() {
        let (_tmp, suite) = suite(r#"["**/*.tir", "**/*.S", "!**/Inputs/**/*"]"#);
        assert_eq!(suite.name, "demo");
        assert!(suite.matches(Path::new("Restructure/loop.tir")));
        assert!(suite.matches(Path::new("obj/golden.S")));
        assert!(!suite.matches(Path::new("README.md")));
        assert!(!suite.matches(Path::new("Diagnostics/Inputs/decl.tir")));
    }

    #[test]
    fn suite_root_relative_exclusion() {
        let (_tmp, suite) = suite(r#"["**/*.tmdl", "!Inputs/**/*"]"#);
        assert!(suite.matches(Path::new("Rust/split.tmdl")));
        assert!(!suite.matches(Path::new("Inputs/simple.tmdl")));
    }

    #[test]
    fn discovers_every_suite_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for dir in [
            "core/checks",
            "backends/riscv/checks",
            "no-suite/checks",
            "target/debug/checks",
            ".hidden/checks",
            "node_modules/pkg/checks",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for dir in [
            "core/checks",
            "target/debug/checks",
            ".hidden/checks",
            "node_modules/pkg/checks",
        ] {
            std::fs::write(
                root.join(dir).join(SUITE_FILE),
                "[suite]\nname = \"s\"\nglob = []",
            )
            .unwrap();
        }
        std::fs::write(
            root.join("backends/riscv/checks").join(SUITE_FILE),
            "[suite]\nname = \"s\"\nglob = []",
        )
        .unwrap();

        let manifests = discover_suites(root).unwrap();
        assert_eq!(
            manifests,
            [
                root.join("backends/riscv/checks").join(SUITE_FILE),
                root.join("core/checks").join(SUITE_FILE),
            ]
        );
    }

    #[test]
    fn invalid_suite_manifest_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join(SUITE_FILE);
        std::fs::write(&manifest, "[suite]\nname = \"s\"\n").unwrap();
        assert!(load_suite(&manifest).is_err());
    }

    #[test]
    fn workspace_root_is_first_manifest_with_workspace_section() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("utils/lit")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []").unwrap();
        std::fs::write(root.join("utils/lit/Cargo.toml"), "[package]").unwrap();

        assert_eq!(
            find_workspace_root(&root.join("utils/lit")),
            Some(root.to_path_buf())
        );
    }

    #[test]
    fn no_filters_runs_everything() {
        assert!(filter_decision("core/Restructure/loop.tir", None, None));
    }

    #[test]
    fn filter_keeps_only_matching_names() {
        let f = re("Restructure");
        assert!(filter_decision("core/Restructure/loop.tir", Some(&f), None));
        assert!(!filter_decision("fcc/Codegen/add.c", Some(&f), None));
    }

    #[test]
    fn filter_out_excludes_matching_names() {
        let f = re("Codegen");
        assert!(!filter_decision("fcc/Codegen/add.c", None, Some(&f)));
        assert!(filter_decision("core/Restructure/loop.tir", None, Some(&f)));
    }

    #[test]
    fn filter_out_wins_over_filter() {
        let keep = re("fcc");
        let drop = re("Codegen");
        assert!(!filter_decision(
            "fcc/Codegen/add.c",
            Some(&keep),
            Some(&drop)
        ));
        assert!(filter_decision("fcc/Lexer/id.c", Some(&keep), Some(&drop)));
    }

    #[test]
    fn detects_unconditional_xfail() {
        assert!(has_unconditional_xfail("// XFAIL: *\n// RUN: tool"));
        assert!(has_unconditional_xfail("// XFAIL: linux, *\n// RUN: tool"));
        assert!(!has_unconditional_xfail("// XFAIL: linux\n// RUN: tool"));
    }

    #[test]
    fn xfail_accepts_failing_case() {
        let case = TestCase {
            name: "xfail.tir".to_string(),
            path: PathBuf::from("xfail.tir"),
            run_lines: vec!["missing-tir-lit-test-tool".to_string()],
            xfail: true,
            ignored: false,
        };

        assert!(run_case(&case, &HashMap::new()).is_ok());
    }

    #[test]
    fn xfail_rejects_passing_case() {
        let case = TestCase {
            name: "xpass.tir".to_string(),
            path: PathBuf::from("xpass.tir"),
            run_lines: Vec::new(),
            xfail: true,
            ignored: false,
        };

        let err = run_case(&case, &HashMap::new()).unwrap_err();
        assert_eq!(err.message(), Some("XPASS: xpass.tir"));
    }

    #[cfg(unix)]
    fn probe_case(run_line: &str) -> (TestCase, HashMap<String, Tool>) {
        let case = TestCase {
            name: "env.tir".to_string(),
            path: PathBuf::from("env.tir"),
            run_lines: vec![run_line.to_string()],
            xfail: false,
            ignored: false,
        };
        let tools = HashMap::from([("probe".to_string(), Tool::path("/usr/bin/printenv"))]);
        (case, tools)
    }

    #[cfg(unix)]
    #[test]
    fn env_prefix_sets_variables_for_a_mapped_tool() {
        let (case, tools) = probe_case("env TIR_LIT_ENV_PROBE=set probe TIR_LIT_ENV_PROBE");
        assert!(run_case(&case, &tools).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn command_without_env_prefix_does_not_see_the_variable() {
        let (case, tools) = probe_case("probe TIR_LIT_ENV_PROBE");
        assert!(run_case(&case, &tools).is_err());
    }

    fn features(tokens: &[&str]) -> HashSet<String> {
        tokens.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn requires_met_runs() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(
            gating_decision(&f, "// REQUIRES: linux\n// RUN: tool"),
            Gate::Run
        );
    }

    #[test]
    fn requires_unmet_ignores() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(
            gating_decision(&f, "// REQUIRES: windows\n// RUN: tool"),
            Gate::Ignore
        );
    }

    #[test]
    fn requires_multiple_features_is_and() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(
            gating_decision(&f, "// REQUIRES: linux, x86_64\n// RUN: tool"),
            Gate::Run
        );
        assert_eq!(
            gating_decision(&f, "// REQUIRES: linux, riscv64\n// RUN: tool"),
            Gate::Ignore
        );
    }

    #[test]
    fn requires_multiple_lines_combine_with_and() {
        let f = features(&["linux", "x86_64"]);
        let contents = "// REQUIRES: linux\n// REQUIRES: x86_64\n// RUN: tool";
        assert_eq!(gating_decision(&f, contents), Gate::Run);

        let contents = "// REQUIRES: linux\n// REQUIRES: riscv64\n// RUN: tool";
        assert_eq!(gating_decision(&f, contents), Gate::Ignore);
    }

    #[test]
    fn requires_negation() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(
            gating_decision(&f, "// REQUIRES: !windows\n// RUN: tool"),
            Gate::Run
        );
        assert_eq!(
            gating_decision(&f, "// REQUIRES: !linux\n// RUN: tool"),
            Gate::Ignore
        );
    }

    #[test]
    fn unsupported_skips_when_present() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(
            gating_decision(&f, "// UNSUPPORTED: x86_64\n// RUN: tool"),
            Gate::Ignore
        );
    }

    #[test]
    fn unsupported_runs_when_absent() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(
            gating_decision(&f, "// UNSUPPORTED: riscv64\n// RUN: tool"),
            Gate::Run
        );
    }

    #[test]
    fn unsupported_negation() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(
            gating_decision(&f, "// UNSUPPORTED: !windows\n// RUN: tool"),
            Gate::Ignore
        );
        assert_eq!(
            gating_decision(&f, "// UNSUPPORTED: !linux\n// RUN: tool"),
            Gate::Run
        );
    }

    #[test]
    fn directive_parsing_is_comment_prefix_agnostic() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(
            gating_decision(&f, "# REQUIRES: windows\n# RUN: tool"),
            gating_decision(&f, "// REQUIRES: windows\n// RUN: tool")
        );
        assert_eq!(
            gating_decision(&f, "# UNSUPPORTED: x86_64\n# RUN: tool"),
            gating_decision(&f, "// UNSUPPORTED: x86_64\n// RUN: tool")
        );
    }

    #[test]
    fn no_directives_runs() {
        let f = features(&["linux", "x86_64"]);
        assert_eq!(gating_decision(&f, "// RUN: tool"), Gate::Run);
    }

    #[test]
    fn host_features_includes_os_and_arch() {
        let f = host_features();
        assert!(f.contains(std::env::consts::OS));
        assert!(!f.is_empty());
    }
}
