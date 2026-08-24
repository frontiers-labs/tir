//! Differential fuzzing of the fcc optimizer: generate random UB-free C
//! programs, compile them under different pass pipelines (and with reference
//! compilers as ground truth), execute natively, and compare observable
//! behavior. A divergence is a miscompile; every program is reproducible from
//! its seed alone.

pub mod generator;
pub mod harness;
pub mod issues;
pub mod reduce;
pub mod report;
pub mod triage;

use anyhow::Context as _;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use xshell::{cmd, Shell};

use harness::Outcome;

/// Mid-end pipelines exercised besides the default. All are semantically
/// neutral orderings of the registered passes; a correct compiler must give
/// identical behavior under each.
const EXTRA_PIPELINES: [&str; 3] = [
    "func.func(thread-state,instcombine,sccp,dce,instcombine,erase-state)",
    "func.func(thread-state,sccp,dce,erase-state)",
    "func.func(thread-state,instcombine,sccp,instcombine,sccp,erase-state)",
];

const CORPUS_DIRS: [&str; 3] = ["fcc/checks", "fcc/tests", "utils/unit-tests/src/fcc/corpus"];

pub struct Options {
    pub seed: u64,
    pub iterations: usize,
    pub corpus: bool,
    pub self_test: bool,
    /// Re-run the defects recorded in this directory and report which are gone.
    pub replay: Option<PathBuf>,
    /// Print one recorded defect as the issue it becomes.
    pub render: Option<PathBuf>,
    /// Recover recorded defects from the open issues fed in on stdin.
    pub extract: Option<PathBuf>,
    /// An already built compiler to test, instead of building a debug one.
    pub fcc: Option<PathBuf>,
}

pub fn run(sh: &Shell, root: &Path, options: &Options) -> anyhow::Result<()> {
    if let Some(file) = &options.render {
        return issues::render(file);
    }
    if let Some(dir) = &options.extract {
        return issues::extract(dir);
    }
    if options.self_test {
        return self_test(sh, root, options.fcc.as_deref());
    }
    let fcc = build_fcc(sh, root, options.fcc.as_deref())?;
    let work_dir = root.join("target/fuzz");
    let failures_dir = work_dir.join("failures");
    std::fs::create_dir_all(&failures_dir)?;

    if let Some(dir) = &options.replay {
        return issues::replay(
            &fcc,
            dir,
            &failures_dir.join("fixed.txt"),
            &reference_variants(),
            &work_dir.join("replay"),
        );
    }
    if options.corpus {
        return run_corpus(sh, root, &fcc, &variants(options.seed), &work_dir);
    }

    println!(
        "fcc-fuzz: {} iterations from seed {}",
        options.iterations, options.seed
    );

    let references = reference_variants();
    let triage_dir = work_dir.join("triage");
    let program_dir = work_dir.join("out");
    std::fs::create_dir_all(&triage_dir)?;
    std::fs::create_dir_all(&program_dir)?;

    let mut filed = BTreeSet::new();
    for index in 0..options.iterations {
        let seed = options.seed + index as u64;
        let source_text = generator::generate(seed);
        let source = work_dir.join(format!("seed-{seed}.c"));
        std::fs::write(&source, &source_text)?;

        for (_, outcome) in harness::run_variants(&fcc, &source, &variants(seed), &program_dir) {
            match outcome {
                Outcome::Agree => {}
                Outcome::Errored { variant, message } => {
                    eprintln!("seed {seed}: {variant} errored: {message}");
                }
                Outcome::Diverged {
                    variant,
                    expected,
                    actual,
                } => {
                    // Only an fcc-versus-fcc divergence has a pipeline to
                    // blame; disagreeing with a reference compiler indicts the
                    // default pipeline, which bisection cannot shrink.
                    let reduced = triage::triage(
                        &fcc,
                        &source_text,
                        variant.strip_prefix("fcc:"),
                        &references,
                        &triage_dir,
                    );
                    let failure = triage::failure(
                        "differential-fuzz",
                        format!("Miscompile: {}", reduced.culprit()),
                        reproduce_command(seed, &reduced),
                        &reduced,
                        &variant,
                        &expected,
                        &actual,
                    );
                    record(&failures_dir, &failure, &mut filed)?;
                }
            }
        }
    }

    println!(
        "fcc-fuzz: {} programs, {} defects",
        options.iterations,
        filed.len()
    );
    if !filed.is_empty() {
        anyhow::bail!("{} defects found", filed.len());
    }
    Ok(())
}

/// Both ways back to this one failure: the seed that generated it, and the
/// minimal case on its own for when the generator has moved on.
fn reproduce_command(seed: u64, reduced: &triage::Reduced) -> String {
    let pipeline = match &reduced.pipeline {
        Some(pipeline) => format!(" --pipeline '{pipeline}'"),
        None => String::new(),
    };
    format!(
        "cargo xtask fcc-fuzz --seed {seed} --iterations 1\n\n\
         # ...or straight at the minimal case above, saved as case.c:\n\
         fcc compile --stage obj --march x86_64{pipeline} -o case.o case.c"
    )
}

/// Hand one defect to the reporter. Signatures repeat within a run whenever two
/// seeds hit the same bug, and one file per signature is what keeps that from
/// becoming two issues.
fn record(
    dir: &Path,
    failure: &report::Failure,
    filed: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    let signature = failure.signature();
    println!("DEFECT {signature}: {}", failure.summary);
    if filed.insert(signature.clone()) {
        std::fs::write(
            dir.join(format!("{signature}.json")),
            serde_json::to_string(failure)?,
        )?;
    }
    Ok(())
}

fn variants(seed: u64) -> Vec<harness::Variant> {
    let mut variants = vec![
        harness::Variant::fcc(None),
        harness::Variant::fcc(Some(extra_pipeline(seed).to_string())),
    ];
    variants.extend(reference_variants());
    variants
}

/// The extra pipeline a program is compiled under, chosen by the program's own
/// seed so that the seed alone identifies both the program and what miscompiled
/// it. Deriving it from the run's base seed instead would make a single-seed
/// reproduce command select a different pipeline than the one that failed.
fn extra_pipeline(seed: u64) -> &'static str {
    EXTRA_PIPELINES[(seed % EXTRA_PIPELINES.len() as u64) as usize]
}

/// Ground truth, where the machine has it.
fn reference_variants() -> Vec<harness::Variant> {
    let mut references = Vec::new();
    if which("gcc") {
        references.push(harness::Variant::gcc());
    }
    if which("clang") {
        references.push(harness::Variant::clang());
    }
    references
}

/// Cross-checks the checked-in C corpus and the GCC torture execute suite:
/// every program that all variants can build must behave identically. Programs
/// fcc cannot build yet are reported by the torture baseline, not here.
fn run_corpus(
    sh: &Shell,
    root: &Path,
    fcc: &Path,
    variants: &[harness::Variant],
    work_dir: &Path,
) -> anyhow::Result<()> {
    let mut files = Vec::new();
    for dir in CORPUS_DIRS {
        collect_c_files(&root.join(dir), &mut files)?;
    }
    files.extend(crate::fcc_torture::execute_corpus(sh, root)?);
    files.sort();
    println!("fcc-fuzz corpus: {} files", files.len());

    let corpus_dir = work_dir.join("corpus");
    let failures_dir = work_dir.join("failures");
    let references = reference_variants();
    let reports = crate::utils::run_parallel("fcc-fuzz corpus", files, |index, file| {
        // Variants name their artifacts after the source stem, which repeats
        // across the corpus, so every case gets its own directory.
        let program_dir = corpus_dir.join(index.to_string());
        let mut agreements = 0;
        let mut failures = Vec::new();
        if std::fs::create_dir_all(&program_dir).is_ok() {
            let source = std::fs::read_to_string(file).unwrap_or_default();
            for (_, outcome) in harness::run_variants(fcc, file, variants, &program_dir) {
                match outcome {
                    Outcome::Agree => agreements += 1,
                    Outcome::Errored { .. } => {}
                    Outcome::Diverged {
                        variant,
                        expected,
                        actual,
                    } => {
                        // A corpus case is curated, so only the pipeline is
                        // shrunk: the path is what names the defect, and the
                        // file is what the reader wants to open.
                        let pipeline =
                            triage::bisect(variant.strip_prefix("fcc:"), &mut |candidate| {
                                triage::diverges(
                                    fcc,
                                    &source,
                                    Some(candidate),
                                    &references,
                                    &program_dir,
                                )
                            });
                        let path = relative(root, file);
                        let reduced = triage::Reduced {
                            artifact: source.clone(),
                            subject: path.clone(),
                            pipeline,
                            shrunk_from: None,
                        };
                        failures.push(triage::failure(
                            "corpus",
                            format!("Miscompile in {path}: {}", reduced.culprit()),
                            corpus_reproduce_command(&path, &reduced),
                            &reduced,
                            &variant,
                            &expected,
                            &actual,
                        ));
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&program_dir);
        }
        (file.display().to_string(), (agreements, failures))
    });

    let compared: usize = reports.iter().map(|(_, (agreements, _))| agreements).sum();
    let mut filed = BTreeSet::new();
    for (_, (_, failures)) in &reports {
        for failure in failures {
            record(&failures_dir, failure, &mut filed)?;
        }
    }
    println!(
        "fcc-fuzz corpus: {compared} variant agreements, {} defects",
        filed.len()
    );
    if !filed.is_empty() {
        anyhow::bail!("{} defects found", filed.len());
    }
    Ok(())
}

fn corpus_reproduce_command(path: &str, reduced: &triage::Reduced) -> String {
    let pipeline = match &reduced.pipeline {
        Some(pipeline) => format!(" --pipeline '{pipeline}'"),
        None => String::new(),
    };
    format!("fcc compile --stage obj --march x86_64{pipeline} -o case.o {path}")
}

/// Corpus paths are reported relative to the checkout so they are clickable;
/// the torture suite lives outside it and keeps its full path.
fn relative(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .display()
        .to_string()
}

/// Resolves the compiler under test. Builds the `ci` profile when none was
/// given: the corpus spends its time running fcc, not compiling it, and this is
/// the profile CI hands over.
fn build_fcc(sh: &Shell, root: &Path, fcc: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(fcc) = fcc {
        return Ok(fcc.to_path_buf());
    }
    cmd!(sh, "cargo build -j4 --profile ci -p fcc --bin fcc").run()?;
    Ok(root.join("target/ci/fcc"))
}

/// Prove the harness catches divergences: run it against a stubbed `gcc` whose
/// "binaries" always print a wrong value, and require the divergence to be
/// detected.
fn self_test(sh: &Shell, root: &Path, fcc: Option<&Path>) -> anyhow::Result<()> {
    let fcc = build_fcc(sh, root, fcc)?;
    let work_dir = root.join("target/fuzz/self-test");
    std::fs::create_dir_all(&work_dir)?;

    let stub_dir = work_dir.join("stub-bin");
    std::fs::create_dir_all(&stub_dir)?;
    let stub = stub_dir.join("gcc");
    // The scratch file goes next to the stub, not into whatever directory the
    // self-test was started from.
    let scratch = stub_dir.join("stub.c");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\n\
             printf '#include <stdio.h>\\nint main(void){{printf(\"999\\\\n\");return 0;}}\\n' > {scratch}\n\
             cc {scratch} -o \"$3\"\n",
            scratch = scratch.display()
        ),
    )?;
    make_executable(&stub)?;

    let source = work_dir.join("prog.c");
    std::fs::write(&source, generator::generate(1))?;

    let saved_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{saved_path}", stub_dir.display()));
    let variants = vec![harness::Variant::fcc(None), harness::Variant::gcc()];
    let outcomes = harness::run_variants(&fcc, &source, &variants, &work_dir);
    // Reduction is exercised on a small hand-written program rather than a
    // generated one: it costs a harness run per candidate deletion, and the
    // point here is that shrinking works, not how far it gets.
    let reduced = triage::triage(
        &fcc,
        SELF_TEST_PROGRAM,
        None,
        &[harness::Variant::gcc()],
        &work_dir,
    );
    std::env::set_var("PATH", saved_path);

    let Some((variant, expected, actual)) =
        outcomes.iter().find_map(|(_, outcome)| match outcome {
            Outcome::Diverged {
                variant,
                expected,
                actual,
            } => Some((variant, expected, actual)),
            _ => None,
        })
    else {
        anyhow::bail!("self-test failed: the harness did not detect the stubbed divergence");
    };

    if reduced.artifact.lines().count() >= SELF_TEST_PROGRAM.lines().count() {
        anyhow::bail!("self-test failed: the divergence was not reduced");
    }

    let failure = triage::failure(
        "differential-fuzz",
        format!("Miscompile: {}", reduced.culprit()),
        reproduce_command(1, &reduced),
        &reduced,
        variant,
        expected,
        actual,
    );
    if report::Failure::from_body(&failure.body()).as_ref() != Some(&failure) {
        anyhow::bail!("self-test failed: a filed issue does not replay back into a defect");
    }

    println!(
        "fcc-fuzz self-test: detection, reduction ({} lines to {}) and replay all work",
        SELF_TEST_PROGRAM.lines().count(),
        reduced.artifact.lines().count()
    );
    Ok(())
}

const SELF_TEST_PROGRAM: &str = "#include <stdio.h>\n\
                                 int main(void) {\n\
                                 \x20   int a = 1;\n\
                                 \x20   int b = 2;\n\
                                 \x20   int c = a + b;\n\
                                 \x20   printf(\"%d\\n\", c);\n\
                                 \x20   return 0;\n\
                                 }\n";

fn make_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).context("chmod stub")?;
    Ok(())
}

fn collect_c_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_c_files(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "c") {
            files.push(path);
        }
    }
    Ok(())
}

fn which(program: &str) -> bool {
    std::env::var("PATH")
        .map(|paths| {
            paths
                .split(':')
                .any(|dir| Path::new(dir).join(program).is_file())
        })
        .unwrap_or(false)
}
