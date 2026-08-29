use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use xshell::{cmd, Shell};

use crate::utils::{run_parallel, run_with_timeout};

const GCC_REPOSITORY: &str = "https://github.com/gcc-mirror/gcc.git";
const GCC_REVISION: &str = "9aab80ddc5b2fa0eef80008e718067ab45f42c50";
const TORTURE_PATH: &str = "gcc/testsuite/gcc.c-torture";
const CHECKOUT_PATH: &str = "target/test-suites/gcc";
const ALLOWLIST_PATH: &str = "fcc/tests/gcc-torture-known-failures.txt";
const EXECUTE_ALLOWLIST_PATH: &str = "fcc/tests/gcc-torture-execute-known-failures.txt";
/// The slowest case (`pr35800.c`, a 35-arm fall-through switch) spends ~45s in
/// instruction selection on a fast desktop, so the budget has to leave room for
/// a CI core several times slower before it reads as a regression. Raising it
/// further is expensive: cases that can never pass hold a whole shard of the
/// run open for the full budget.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(300);
/// Cases that take more than half of `COMPILE_TIMEOUT` on a fast desktop. A
/// slower CI core can push them over the budget, so neither outcome counts as a
/// baseline change. Keep the list minimal: every entry is codegen coverage
/// traded away for a stable signal.
const TIMEOUT_MARGINAL: &[&str] = &[
    "compile/pr34093.c",
    "execute/pr35800.c",
    "execute/pr48809.c",
];

/// Compiles every torture case through codegen and compares the failures
/// against the recorded baseline. `fcc` reuses an already built compiler
/// instead of building one. The baseline is recorded against the `ci` profile,
/// so building anything else here would report the difference as a regression.
pub fn run(sh: &Shell, root: &Path, bless: bool, fcc: Option<&Path>) -> anyhow::Result<()> {
    let corpus = fetch_corpus(sh, root)?;
    let fcc = match fcc {
        Some(fcc) => fcc.to_path_buf(),
        None => {
            cmd!(sh, "cargo build --profile ci -p fcc --bin fcc").run()?;
            root.join("target/ci/fcc")
        }
    };

    let mut files = Vec::new();
    collect_c_files(&corpus.join("compile"), &mut files)?;
    collect_c_files(&corpus.join("execute"), &mut files)?;
    files.sort();

    let results = run_compile(&fcc, &corpus, files)?;
    if !check(&root.join(ALLOWLIST_PATH), results, bless)? {
        anyhow::bail!("GCC torture baseline changed");
    }
    Ok(())
}

/// Fetches the pinned torture checkout and returns the execute cases fcc is
/// expected to compile and run correctly, which the differential fuzzer and
/// the compile-time bench use as a corpus.
pub fn execute_corpus(sh: &Shell, root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let corpus = fetch_corpus(sh, root)?;
    let mut known_failures = parse_allowlist(&fs::read_to_string(root.join(ALLOWLIST_PATH))?)?;
    known_failures.extend(parse_allowlist(&fs::read_to_string(
        root.join(EXECUTE_ALLOWLIST_PATH),
    )?)?);
    let mut files = Vec::new();
    collect_c_files(&corpus.join("execute"), &mut files)?;
    files.retain(|file| !known_failures.contains(&relative_path(&corpus, file)));
    files.sort();
    Ok(files)
}

fn fetch_corpus(sh: &Shell, root: &Path) -> anyhow::Result<PathBuf> {
    let checkout = root.join(CHECKOUT_PATH);
    if !checkout.join(".git").is_dir() {
        fs::create_dir_all(&checkout)?;
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
    Ok(checkout.join(TORTURE_PATH))
}

/// Compares the run against its allowlist, or rewrites the allowlist when
/// blessing. Returns whether the baseline still holds.
fn check(allowlist_path: &Path, results: Vec<(String, bool)>, bless: bool) -> anyhow::Result<bool> {
    let failures = results
        .iter()
        .filter_map(|(path, passed)| {
            (!passed && !TIMEOUT_MARGINAL.contains(&path.as_str())).then_some(path.as_str())
        })
        .collect::<BTreeSet<_>>();
    if bless {
        let contents = if failures.is_empty() {
            String::new()
        } else {
            format!(
                "{}\n",
                failures.iter().copied().collect::<Vec<_>>().join("\n")
            )
        };
        fs::write(allowlist_path, contents)?;
        println!("recorded {} known GCC torture failures", failures.len());
        return Ok(true);
    }

    let expected = parse_allowlist(&fs::read_to_string(allowlist_path)?)?;
    let classification = classify_results(&expected, TIMEOUT_MARGINAL, &results);
    println!(
        "GCC torture: {}/{} passed, {} expected failures",
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

/// Runs each case all the way through codegen: a case passes only when fcc
/// emits assembly for it, so an instruction selection failure is a test
/// failure. CI builds fcc with debug assertions, which also runs the IR
/// verifier after every pass, so invalid IR fails here too.
fn run_compile(
    fcc: &Path,
    corpus: &Path,
    files: Vec<PathBuf>,
) -> anyhow::Result<Vec<(String, bool)>> {
    Ok(run_parallel("GCC torture", files, |_, file| {
        let mut command = Command::new(fcc);
        command
            .args([
                "compile",
                "-std=gnu17",
                "--stage",
                "asm",
                "--march",
                "x86_64",
                "-o",
                "-",
            ])
            .arg(file)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        (
            relative_path(corpus, file),
            run_with_timeout(&mut command, COMPILE_TIMEOUT),
        )
    }))
}

fn relative_path(corpus: &Path, file: &Path) -> String {
    file.strip_prefix(corpus)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
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
    marginal: &[&str],
    results: &[(String, bool)],
) -> Classification {
    let mut unexpected_failures = Vec::new();
    let mut stale_failures = Vec::new();
    let result_paths = results
        .iter()
        .map(|(path, _)| path.as_str())
        .collect::<BTreeSet<_>>();
    for (path, passed) in results {
        if marginal.contains(&path.as_str()) {
            continue;
        }
        match (*passed, expected.contains(path)) {
            (false, false) => unexpected_failures.push(path.clone()),
            (true, true) => stale_failures.push(path.clone()),
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

    use super::{classify_results, parse_allowlist};

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
    fn marginal_case_is_neither_a_regression_nor_stale() {
        let marginal = ["execute/slow.c"];
        let failed = classify_results(
            &BTreeSet::new(),
            &marginal,
            &[("execute/slow.c".to_string(), false)],
        );
        assert!(failed.unexpected_failures.is_empty());
        let expected = BTreeSet::from(["execute/slow.c".to_string()]);
        let passed = classify_results(
            &expected,
            &marginal,
            &[("execute/slow.c".to_string(), true)],
        );
        assert!(passed.stale_failures.is_empty());
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
}
