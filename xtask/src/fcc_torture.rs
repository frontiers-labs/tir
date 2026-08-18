use std::collections::BTreeSet;
use std::fs;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use xshell::{cmd, Shell};

const GCC_REPOSITORY: &str = "https://github.com/gcc-mirror/gcc.git";
const GCC_REVISION: &str = "9aab80ddc5b2fa0eef80008e718067ab45f42c50";
const TORTURE_PATH: &str = "gcc/testsuite/gcc.c-torture";
/// The slowest execute case (`pr35800.c`, a 35-arm fall-through switch) spends
/// ~45s in instruction selection on a fast desktop, so the budget has to leave
/// room for a CI core several times slower before it reads as a regression.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(300);
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const PROGRESS_CASE_INTERVAL: usize = 250;
const PROGRESS_TIME_INTERVAL: Duration = Duration::from_secs(30);
const UNSTABLE_EXECUTE_TESTS: &[&str] = &["execute/pr34099-2.c"];

pub fn run(sh: &Shell, root: &Path, bless: bool) -> anyhow::Result<()> {
    let checkout = root.join("target/test-suites/gcc");
    fetch_gcc(sh, &checkout)?;

    cmd!(sh, "cargo build --release -p fcc --bin fcc").run()?;
    let fcc = root.join("target/release/fcc");
    let corpus = checkout.join(TORTURE_PATH);
    let mut execute_files = Vec::new();
    collect_c_files(&corpus.join("execute"), &mut execute_files)?;
    execute_files.sort();

    let mut files = execute_files.clone();
    collect_c_files(&corpus.join("compile"), &mut files)?;
    files.sort();

    let parser = check_phase(
        "parser",
        &root.join("fcc/tests/gcc-torture-known-failures.txt"),
        &[],
        run_parser(&fcc, &corpus, files)?,
        bless,
    )?;
    let workspace = root.join("target/test-suites/gcc-torture-execute");
    let execute = check_phase(
        "execute",
        &root.join("fcc/tests/gcc-torture-execute-known-failures.txt"),
        UNSTABLE_EXECUTE_TESTS,
        run_execute(&fcc, &corpus, &workspace, execute_files)?,
        bless,
    )?;
    if !parser || !execute {
        anyhow::bail!("GCC torture baseline changed");
    }
    Ok(())
}

/// Compares one phase against its allowlist, or rewrites the allowlist when
/// blessing. Returns whether the baseline still holds.
fn check_phase(
    label: &str,
    allowlist_path: &Path,
    unstable: &[&str],
    results: Vec<(String, bool)>,
    bless: bool,
) -> anyhow::Result<bool> {
    let mut failures = results
        .iter()
        .filter_map(|(path, passed)| (!passed).then_some(path.as_str()))
        .collect::<BTreeSet<_>>();
    if bless {
        failures.extend(unstable.iter().copied());
        let contents = if failures.is_empty() {
            String::new()
        } else {
            format!(
                "{}\n",
                failures.iter().copied().collect::<Vec<_>>().join("\n")
            )
        };
        fs::write(allowlist_path, contents)?;
        println!(
            "recorded {} known GCC torture {label} failures",
            failures.len()
        );
        return Ok(true);
    }

    let expected = parse_allowlist(&fs::read_to_string(allowlist_path)?)?;
    if let Some(path) = unstable.iter().find(|path| !expected.contains(**path)) {
        anyhow::bail!("unstable GCC torture test is not an expected failure: {path}");
    }
    let classification = classify_results(&expected, unstable, &results);
    println!(
        "GCC torture {label}: {}/{} passed, {} expected failures",
        results.len() - failures.len(),
        results.len(),
        expected.len()
    );
    print_paths("unexpected failures", &classification.unexpected_failures);
    print_paths("stale failures", &classification.stale_failures);
    print_paths("missing allowlist entries", &classification.missing_entries);
    Ok(classification.unexpected_failures.is_empty()
        && classification.stale_failures.is_empty()
        && classification.missing_entries.is_empty())
}

fn fetch_gcc(sh: &Shell, checkout: &Path) -> anyhow::Result<()> {
    if !checkout.join(".git").is_dir() {
        fs::create_dir_all(checkout)?;
        cmd!(sh, "git -C {checkout} init").run()?;
        cmd!(sh, "git -C {checkout} remote add origin {GCC_REPOSITORY}").run()?;
        cmd!(sh, "git -C {checkout} sparse-checkout set {TORTURE_PATH}").run()?;
    }
    cmd!(
        sh,
        "git -C {checkout} fetch --depth 1 --filter=blob:none origin {GCC_REVISION}"
    )
    .run()?;
    cmd!(sh, "git -C {checkout} checkout --detach FETCH_HEAD").run()?;
    Ok(())
}

fn collect_c_files(directory: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_c_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "c") {
            files.push(path);
        }
    }
    Ok(())
}

fn run_parser(
    fcc: &Path,
    corpus: &Path,
    files: Vec<PathBuf>,
) -> anyhow::Result<Vec<(String, bool)>> {
    Ok(run_parallel("parser", files, |file| {
        let passed = Command::new(fcc)
            .args(["compile", "-std=gnu17", "--stage", "ast", "-o", "-"])
            .arg(file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        (relative_path(corpus, file), passed)
    }))
}

/// Compiles and links each execute test with `cc`, then runs it: the GCC
/// torture execute tests report failure by aborting, so a zero exit status is
/// the whole verdict.
fn run_execute(
    fcc: &Path,
    corpus: &Path,
    workspace: &Path,
    files: Vec<PathBuf>,
) -> anyhow::Result<Vec<(String, bool)>> {
    fs::create_dir_all(workspace)?;
    let cases = pair_libraries(&files);
    Ok(run_parallel("execute", cases, |(test, library)| {
        let path = relative_path(corpus, test);
        let program = workspace.join(path.replace('/', "_"));
        let mut command = Command::new(fcc);
        command.args(["cc", "-std=gnu17"]).arg(test);
        if let Some(library) = library {
            command.arg(library);
        }
        command
            .arg("-o")
            .arg(&program)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = fs::remove_file(&program);
        let passed = run_with_timeout(&mut command, COMPILE_TIMEOUT)
            && run_with_timeout(
                Command::new(&program)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
                RUN_TIMEOUT,
            );
        let _ = fs::remove_file(&program);
        (path, passed)
    }))
}

/// Pairs each test with its `-lib.c` companion, which GCC links in separately
/// for the `builtins` tests.
fn pair_libraries(files: &[PathBuf]) -> Vec<(PathBuf, Option<PathBuf>)> {
    let known = files.iter().collect::<BTreeSet<_>>();
    files
        .iter()
        .filter(|file| !file.to_string_lossy().ends_with("-lib.c"))
        .map(|file| {
            let library = file.with_file_name(format!(
                "{}-lib.c",
                file.file_stem().unwrap().to_string_lossy()
            ));
            let library = known.contains(&library).then_some(library);
            (file.clone(), library)
        })
        .collect()
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> bool {
    #[cfg(unix)]
    command.process_group(0);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn relative_path(corpus: &Path, file: &Path) -> String {
    file.strip_prefix(corpus)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn run_parallel<T: Send + Sync>(
    label: &str,
    items: Vec<T>,
    task: impl Fn(&T) -> (String, bool) + Sync,
) -> Vec<(String, bool)> {
    let items = Arc::new(items);
    let next = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(Vec::with_capacity(items.len())));
    let workers = std::thread::available_parallelism().map_or(1, usize::from);
    let task = &task;
    let (completed_sender, completed_receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let items = Arc::clone(&items);
            let next = Arc::clone(&next);
            let results = Arc::clone(&results);
            let completed_sender = completed_sender.clone();
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(index) else {
                    break;
                };
                let result = task(item);
                results.lock().unwrap().push(result);
                let _ = completed_sender.send(());
            });
        }
        drop(completed_sender);

        let mut completed = 0;
        while completed < items.len() {
            match completed_receiver.recv_timeout(PROGRESS_TIME_INTERVAL) {
                Ok(()) => {
                    completed += 1;
                    if !should_report_progress(completed, items.len()) {
                        continue;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            println!(
                "GCC torture {label} progress: {completed}/{} cases",
                items.len()
            );
        }
    });
    let mut results = Arc::into_inner(results).unwrap().into_inner().unwrap();
    results.sort_by(|left, right| left.0.cmp(&right.0));
    results
}

fn should_report_progress(completed: usize, total: usize) -> bool {
    completed == total || completed.is_multiple_of(PROGRESS_CASE_INTERVAL)
}

fn print_paths(label: &str, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    eprintln!("{label}:");
    for path in paths {
        eprintln!("  {path}");
    }
}

struct Classification {
    unexpected_failures: Vec<String>,
    stale_failures: Vec<String>,
    missing_entries: Vec<String>,
}

fn classify_results(
    expected: &BTreeSet<String>,
    unstable: &[&str],
    results: &[(String, bool)],
) -> Classification {
    let mut unexpected_failures = Vec::new();
    let mut stale_failures = Vec::new();
    let result_paths = results
        .iter()
        .map(|(path, _)| path.as_str())
        .collect::<BTreeSet<_>>();
    for (path, passed) in results {
        match (*passed, expected.contains(path)) {
            (false, false) => unexpected_failures.push(path.clone()),
            (true, true) if !unstable.contains(&path.as_str()) => stale_failures.push(path.clone()),
            _ => {}
        }
    }
    Classification {
        unexpected_failures,
        stale_failures,
        missing_entries: expected
            .iter()
            .filter(|path| !result_paths.contains(path.as_str()))
            .cloned()
            .collect(),
    }
}

fn parse_allowlist(contents: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for line in contents.lines() {
        let path = line.trim();
        if path.is_empty() || path.starts_with('#') {
            continue;
        }
        if !paths.insert(path.to_string()) {
            anyhow::bail!("duplicate GCC torture allowlist entry: {path}");
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::process::Command;
    use std::time::Duration;

    #[cfg(unix)]
    use super::run_with_timeout;
    use super::{classify_results, pair_libraries, parse_allowlist, should_report_progress};

    #[test]
    fn library_companion_is_linked_with_its_test() {
        let files = vec![
            PathBuf::from("execute/builtins/abs-1-lib.c"),
            PathBuf::from("execute/builtins/abs-1.c"),
        ];
        assert_eq!(
            pair_libraries(&files),
            [(
                PathBuf::from("execute/builtins/abs-1.c"),
                Some(PathBuf::from("execute/builtins/abs-1-lib.c"))
            )]
        );
    }

    #[test]
    fn test_without_companion_is_compiled_alone() {
        let files = vec![PathBuf::from("execute/20000112-1.c")];
        assert_eq!(
            pair_libraries(&files),
            [(PathBuf::from("execute/20000112-1.c"), None)]
        );
    }

    #[test]
    fn unlisted_failure_is_a_regression() {
        let result = classify_results(
            &BTreeSet::new(),
            &[],
            &[("compile/new.c".to_string(), false)],
        );
        assert_eq!(result.unexpected_failures, ["compile/new.c"]);
        assert!(result.stale_failures.is_empty());
    }

    #[test]
    fn listed_success_is_stale() {
        let expected = BTreeSet::from(["execute/fixed.c".to_string()]);
        let result = classify_results(&expected, &[], &[("execute/fixed.c".to_string(), true)]);
        assert_eq!(result.stale_failures, ["execute/fixed.c"]);
        assert!(result.unexpected_failures.is_empty());
    }

    #[test]
    fn unstable_success_is_not_stale() {
        let expected = BTreeSet::from(["execute/unstable.c".to_string()]);
        let result = classify_results(
            &expected,
            &["execute/unstable.c"],
            &[("execute/unstable.c".to_string(), true)],
        );
        assert!(result.stale_failures.is_empty());
        assert!(result.unexpected_failures.is_empty());
    }

    #[test]
    fn duplicate_allowlist_entry_is_rejected() {
        assert!(parse_allowlist("compile/a.c\ncompile/a.c\n").is_err());
    }

    #[test]
    fn missing_allowlist_path_is_reported() {
        let expected = BTreeSet::from(["compile/removed.c".to_string()]);
        let result = classify_results(&expected, &[], &[]);
        assert_eq!(result.missing_entries, ["compile/removed.c"]);
    }

    #[cfg(unix)]
    #[test]
    fn timed_command_runs_in_its_own_process_group() {
        let mut command = Command::new("sh");
        command.args(["-c", r#"test "$(ps -o pgid= -p $$ | tr -d ' ')" = "$$""#]);
        assert!(run_with_timeout(&mut command, Duration::from_secs(1)));
    }

    #[test]
    fn progress_is_reported_at_intervals_and_completion() {
        assert!(!should_report_progress(249, 1_000));
        assert!(should_report_progress(250, 1_000));
        assert!(!should_report_progress(999, 1_000));
        assert!(should_report_progress(1_000, 1_000));
    }
}
