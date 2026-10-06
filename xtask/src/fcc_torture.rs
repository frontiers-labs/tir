use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use libtest_mimic::{Arguments, Trial};
use xshell::{cmd, Shell};

use crate::utils::{collect_c_files, run_parallel, run_with_timeout};

const GCC_REPOSITORY: &str = "https://github.com/gcc-mirror/gcc.git";
const GCC_REVISION: &str = "9aab80ddc5b2fa0eef80008e718067ab45f42c50";
const TORTURE_PATH: &str = "gcc/testsuite/gcc.c-torture";
const CHECKOUT_PATH: &str = "target/test-suites/gcc";
const ALLOWLIST_PATH: &str = "fcc/tests/gcc-torture-known-failures.txt";
const EXECUTE_ALLOWLIST_PATH: &str = "fcc/tests/gcc-torture-execute-known-failures.txt";
const KNOWN_FAILURES: &str = include_str!("../../fcc/tests/gcc-torture-known-failures.txt");
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

/// Runs prepared GCC torture cases as individual Cargo tests. The caller supplies
/// `TIR_TEST_BIN_DIR` and `TIR_TEST_CORPUS_DIR`; this entry point never builds or
/// downloads tools or sources.
pub fn harness_main() {
    let args = Arguments::from_args();
    let trials = harness_trials().unwrap_or_else(|error| {
        vec![Trial::test("gcc-torture/baseline", move || {
            Err(error.to_string().into())
        })]
    });
    libtest_mimic::run(&args, trials).exit();
}

fn harness_trials() -> anyhow::Result<Vec<Trial>> {
    let fcc = std::env::var_os("TIR_TEST_BIN_DIR").map(|directory| {
        PathBuf::from(directory).join(format!("fcc{}", std::env::consts::EXE_SUFFIX))
    });
    let corpus = std::env::var_os("TIR_TEST_CORPUS_DIR").map(PathBuf::from);
    let Some((fcc, corpus)) = fcc.zip(corpus).filter(|(fcc, corpus)| {
        fcc.is_file() && corpus.join("compile").is_dir() && corpus.join("execute").is_dir()
    }) else {
        eprintln!("GCC torture skipped: supply built fcc in TIR_TEST_BIN_DIR and prepared gcc.c-torture in TIR_TEST_CORPUS_DIR");
        return Ok(vec![Trial::test(
            "gcc-torture/requires-tools-and-corpus",
            || Ok(()),
        )
        .with_ignored_flag(true)]);
    };
    let expected = parse_allowlist(KNOWN_FAILURES)?;
    let files = corpus_files(&corpus)?;
    anyhow::ensure!(
        !files.is_empty(),
        "prepared GCC torture corpus has no cases"
    );
    let paths = files
        .iter()
        .map(|file| relative_path(&corpus, file))
        .collect::<BTreeSet<_>>();
    let mut trials = Vec::new();
    for path in expected.difference(&paths) {
        let path = path.clone();
        trials.push(Trial::test(format!("gcc-torture/{path}"), move || {
            Err(format!("missing allowlist entry: {path}").into())
        }));
    }
    for file in files {
        let path = relative_path(&corpus, &file);
        let known_failure = expected.contains(&path);
        let fcc = fcc.clone();
        trials.push(Trial::test(format!("gcc-torture/{path}"), move || {
            let passed = compile_case(&fcc, &file);
            match case_failure(&path, passed, known_failure, TIMEOUT_MARGINAL) {
                Some(label) => Err(format!("{label}: {path}").into()),
                None => Ok(()),
            }
        }));
    }
    Ok(trials)
}

/// Rewrites the baseline from the pinned corpus using the supplied compiler,
/// or a CI-profile build when none is supplied.
pub fn bless(sh: &Shell, root: &Path, fcc: Option<&Path>) -> anyhow::Result<()> {
    let corpus = fetch_corpus(sh, root)?;
    let fcc = match fcc {
        Some(fcc) => fcc.to_path_buf(),
        None => {
            cmd!(sh, "cargo build --profile ci -p fcc --bin fcc").run()?;
            root.join("target/ci/fcc")
        }
    };

    let results = run_parallel("GCC torture", corpus_files(&corpus)?, |_, file| {
        (relative_path(&corpus, file), compile_case(&fcc, file))
    });
    let failures = results
        .into_iter()
        .filter_map(|(path, passed)| {
            (!passed && !TIMEOUT_MARGINAL.contains(&path.as_str())).then_some(path)
        })
        .collect::<Vec<_>>();
    let mut contents = failures.join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    fs::write(root.join(ALLOWLIST_PATH), contents)?;
    println!("recorded {} known GCC torture failures", failures.len());
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

/// Fetches the pinned GCC sources for orchestration and returns gcc.c-torture.
pub fn fetch_corpus(sh: &Shell, root: &Path) -> anyhow::Result<PathBuf> {
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

fn corpus_files(corpus: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_c_files(&corpus.join("compile"), &mut files)?;
    collect_c_files(&corpus.join("execute"), &mut files)?;
    files.sort();
    Ok(files)
}

fn compile_case(fcc: &Path, file: &Path) -> bool {
    let mut command = Command::new(fcc);
    command
        .args([
            "compile",
            "-O2",
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
    run_with_timeout(&mut command, COMPILE_TIMEOUT)
}

fn case_failure(
    path: &str,
    passed: bool,
    expected: bool,
    marginal: &[&str],
) -> Option<&'static str> {
    if marginal.contains(&path) {
        return None;
    }
    match (passed, expected) {
        (false, false) => Some("unexpected failure"),
        (true, true) => Some("stale failure"),
        _ => None,
    }
}

fn relative_path(corpus: &Path, file: &Path) -> String {
    file.strip_prefix(corpus)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
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
    use super::{case_failure, parse_allowlist};

    #[test]
    fn unlisted_failure_is_a_regression() {
        assert_eq!(
            case_failure("compile/new.c", false, false, &[]),
            Some("unexpected failure")
        );
    }

    #[test]
    fn listed_success_is_stale() {
        assert_eq!(
            case_failure("execute/fixed.c", true, true, &[]),
            Some("stale failure")
        );
    }

    #[test]
    fn marginal_case_is_neither_a_regression_nor_stale() {
        let marginal = ["execute/slow.c"];
        assert_eq!(
            case_failure("execute/slow.c", false, false, &marginal),
            None
        );
        assert_eq!(case_failure("execute/slow.c", true, true, &marginal), None);
    }

    #[test]
    fn duplicate_allowlist_entry_is_rejected() {
        assert!(parse_allowlist("compile/a.c\ncompile/a.c\n").is_err());
    }
}
