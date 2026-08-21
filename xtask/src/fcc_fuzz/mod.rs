//! Differential fuzzing of the fcc optimizer: generate random UB-free C
//! programs, compile them under different pass pipelines (and with reference
//! compilers as ground truth), execute natively, and compare observable
//! behavior. A divergence is a miscompile; every program is reproducible from
//! its seed alone.

pub mod generator;
pub mod harness;

use anyhow::Context as _;
use std::path::{Path, PathBuf};
use xshell::{cmd, Shell};

use harness::Outcome;

/// Mid-end pipelines exercised besides the default. All are semantically
/// neutral orderings of the registered passes; a correct compiler must give
/// identical behavior under each.
const EXTRA_PIPELINES: [&str; 3] = [
    "func.func(thread-state,instcombine,dce,instcombine,erase-state)",
    "func.func(thread-state,dce,erase-state)",
    "func.func(thread-state,instcombine,instcombine,erase-state)",
];

const CORPUS_DIRS: [&str; 3] = ["fcc/checks", "fcc/tests", "utils/unit-tests/src/fcc/corpus"];

pub struct Options {
    pub seed: u64,
    pub iterations: usize,
    pub corpus: bool,
    pub self_test: bool,
}

pub fn run(sh: &Shell, root: &Path, options: &Options) -> anyhow::Result<()> {
    if options.self_test {
        return self_test(sh, root);
    }
    cmd!(sh, "cargo build -j4 -p fcc --bin fcc").run()?;
    let fcc = root.join("target/debug/fcc");
    let work_dir = root.join("target/fuzz");
    let failures_dir = work_dir.join("failures");
    std::fs::create_dir_all(&failures_dir)?;

    let variants = variants(options.seed);
    if options.corpus {
        return run_corpus(root, &fcc, &variants, &work_dir);
    }

    println!(
        "fcc-fuzz: {} iterations from seed {}, variants: {:?}",
        options.iterations,
        options.seed,
        variants.iter().map(|v| &v.name).collect::<Vec<_>>()
    );

    let mut divergences = 0;
    for index in 0..options.iterations {
        let seed = options.seed + index as u64;
        let source_text = generator::generate(seed);
        let source = work_dir.join(format!("seed-{seed}.c"));
        std::fs::write(&source, &source_text)?;

        let program_dir = work_dir.join("out");
        std::fs::create_dir_all(&program_dir)?;
        for (_, outcome) in harness::run_variants(&fcc, &source, &variants, &program_dir) {
            match outcome {
                Outcome::Agree => {}
                Outcome::Errored { variant, message } => {
                    eprintln!("seed {seed}: {variant} errored: {message}");
                }
                Outcome::Diverged {
                    variant,
                    expected,
                    actual,
                    ..
                } => {
                    divergences += 1;
                    let kept = failures_dir.join(format!("seed-{seed}.c"));
                    std::fs::write(&kept, &source_text)?;
                    println!("DIVERGENCE seed={seed} variant={variant}");
                    if let Some(line) = harness::first_difference(&expected, &actual) {
                        println!("  {line}");
                    }
                    println!("  expected: {}", expected.describe());
                    println!("  actual:   {}", actual.describe());
                    println!("  program saved to {}", kept.display());
                }
            }
        }
    }

    println!(
        "fcc-fuzz: {} programs, {divergences} divergences",
        options.iterations
    );
    if divergences > 0 {
        anyhow::bail!("{divergences} divergences found");
    }
    Ok(())
}

fn variants(seed: u64) -> Vec<harness::Variant> {
    let mut variants = vec![harness::Variant::fcc(None)];
    let extra = EXTRA_PIPELINES[(seed % EXTRA_PIPELINES.len() as u64) as usize];
    variants.push(harness::Variant::fcc(Some(extra.to_string())));
    if which("gcc") {
        variants.push(harness::Variant::gcc());
    }
    if which("clang") {
        variants.push(harness::Variant::clang());
    }
    variants
}

fn run_corpus(
    root: &Path,
    fcc: &Path,
    variants: &[harness::Variant],
    work_dir: &Path,
) -> anyhow::Result<()> {
    let mut files = Vec::new();
    for dir in CORPUS_DIRS {
        collect_c_files(&root.join(dir), &mut files)?;
    }
    files.sort();
    println!("fcc-fuzz corpus: {} files", files.len());

    let mut compared = 0;
    let mut divergences = 0;
    for file in &files {
        let program_dir = work_dir.join("corpus");
        std::fs::create_dir_all(&program_dir)?;
        for (_, outcome) in harness::run_variants(fcc, file, variants, &program_dir) {
            match outcome {
                Outcome::Agree => compared += 1,
                Outcome::Errored { .. } => {}
                Outcome::Diverged {
                    variant,
                    expected,
                    actual,
                    ..
                } => {
                    divergences += 1;
                    println!("DIVERGENCE {} variant={variant}", file.display());
                    if let Some(line) = harness::first_difference(&expected, &actual) {
                        println!("  {line}");
                    }
                    println!("  expected: {}", expected.describe());
                    println!("  actual:   {}", actual.describe());
                }
            }
        }
    }
    println!("fcc-fuzz corpus: {compared} variant agreements, {divergences} divergences");
    if divergences > 0 {
        anyhow::bail!("{divergences} divergences found");
    }
    Ok(())
}

/// Prove the harness catches divergences: run it against a stubbed `gcc` whose
/// "binaries" always print a wrong value, and require the divergence to be
/// detected.
fn self_test(sh: &Shell, root: &Path) -> anyhow::Result<()> {
    cmd!(sh, "cargo build -j4 -p fcc --bin fcc").run()?;
    let fcc = root.join("target/debug/fcc");
    let work_dir = root.join("target/fuzz/self-test");
    std::fs::create_dir_all(&work_dir)?;

    let stub_dir = work_dir.join("stub-bin");
    std::fs::create_dir_all(&stub_dir)?;
    let stub = stub_dir.join("gcc");
    std::fs::write(
        &stub,
        "#!/bin/sh\n\
         printf '#include <stdio.h>\\nint main(void){printf(\"999\\\\n\");return 0;}\\n' > stub.c\n\
         cc stub.c -o \"$3\"\n",
    )?;
    make_executable(&stub)?;

    let source = work_dir.join("prog.c");
    std::fs::write(&source, generator::generate(1))?;

    let saved_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{saved_path}", stub_dir.display()));
    let variants = vec![harness::Variant::fcc(None), harness::Variant::gcc()];
    let outcomes = harness::run_variants(&fcc, &source, &variants, &work_dir);
    std::env::set_var("PATH", saved_path);

    let detected = outcomes
        .iter()
        .any(|(_, outcome)| matches!(outcome, Outcome::Diverged { .. }));
    if !detected {
        anyhow::bail!("self-test failed: the harness did not detect the stubbed divergence");
    }
    println!("fcc-fuzz self-test: divergence detection works");
    Ok(())
}

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
