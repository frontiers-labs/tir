use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{ensure, Context, Result};
use clap::Args;
use xshell::{cmd, Shell};

const BUNDLE: &str = "test-bundle";

#[derive(Args, Default)]
pub struct Options {
    /// Cargo build profile. Defaults to the profile of this xtask executable.
    #[arg(long)]
    pub profile: Option<String>,
    /// Include the pinned GCC torture corpus. Requires the ci build profile.
    #[arg(long)]
    pub torture: bool,
    /// Run cargo test even when cargo-nextest is installed.
    #[arg(long, conflicts_with = "archive")]
    pub cargo_test: bool,
    /// Run a prepared nextest archive from a checkout of the packed commit.
    #[arg(long, conflicts_with_all = ["profile", "torture"])]
    pub archive: Option<PathBuf>,
    /// Arguments for the test runner, after `--`.
    #[arg(last = true)]
    pub runner_args: Vec<String>,
}

#[derive(Args)]
pub struct PackOptions {
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub torture: bool,
    #[arg(long)]
    pub output: PathBuf,
}

struct Built {
    profile: String,
    binaries: BTreeSet<String>,
    binary_directory: PathBuf,
    corpus: Option<PathBuf>,
}

pub fn run(root: &Path, options: Options) -> Result<()> {
    if let Some(archive) = &options.archive {
        return run_archive(root, archive, &options.runner_args);
    }
    let built = build(root, options.profile.as_deref(), options.torture)?;
    let sh = Shell::new()?;
    sh.change_dir(root);
    let profile = &built.profile;
    let runner_args = &options.runner_args;
    let runner = if !options.cargo_test && has_nextest() {
        cmd!(
            sh,
            "cargo nextest run --cargo-profile {profile} --workspace --locked {runner_args...}"
        )
    } else {
        if !options.cargo_test {
            eprintln!("cargo-nextest is not installed; running cargo test");
        }
        cmd!(
            sh,
            "cargo test --profile {profile} --workspace --locked {runner_args...}"
        )
    }
    .env("TIR_TEST_BIN_DIR", &built.binary_directory);
    let runner = match &built.corpus {
        Some(corpus) => runner.env("TIR_TEST_CORPUS_DIR", corpus),
        None => runner.env_remove("TIR_TEST_CORPUS_DIR"),
    };
    let mut runner = Command::from(runner);
    ensure!(runner.status()?.success(), "test run did not succeed");
    Ok(())
}

fn run_archive(root: &Path, archive: &Path, runner_args: &[String]) -> Result<()> {
    let extracted = tir_adt::TempDir::with_prefix("tir-tests-")?;
    let bundle = extracted.path().join("target").join(BUNDLE);
    let sh = Shell::new()?;
    let extracted_path = extracted.path();
    let status = Command::from(
        cmd!(sh, "cargo nextest run --archive-file {archive} --extract-to {extracted_path} --workspace-remap {root} {runner_args...}")
            .env("TIR_TEST_BIN_DIR", bundle.join("bin"))
            .env("TIR_TEST_CORPUS_DIR", bundle.join("gcc-corpus")),
    )
    .status()?;
    ensure!(status.success(), "test run did not succeed");
    Ok(())
}

pub fn pack(root: &Path, options: PackOptions) -> Result<()> {
    ensure!(has_nextest(), "test-pack requires cargo-nextest");
    let built = build(root, options.profile.as_deref(), options.torture)?;
    let bundle = target_directory(root)?.join(BUNDLE);
    if bundle.exists() {
        fs::remove_dir_all(&bundle)?;
    }
    // Nextest archives only the binaries of packages with integration tests,
    // so the tools travel with the torture corpus.
    fs::create_dir_all(bundle.join("bin"))?;
    for name in &built.binaries {
        let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        let (source, staged) = (
            built.binary_directory.join(&name),
            bundle.join("bin").join(&name),
        );
        if fs::hard_link(&source, &staged).is_err() {
            fs::copy(&source, &staged)?;
        }
    }
    if let Some(corpus) = &built.corpus {
        copy_tree(corpus, &bundle.join("gcc-corpus"))?;
    }
    let output = std::env::current_dir()?.join(options.output);
    let sh = Shell::new()?;
    sh.change_dir(root);
    let profile = &built.profile;
    let status = Command::from(cmd!(
        sh,
        "cargo nextest archive --workspace --locked --cargo-profile {profile} --archive-file {output}"
    ))
    .status()?;
    ensure!(status.success(), "cargo nextest archive failed");
    Ok(())
}

pub(crate) fn has_nextest() -> bool {
    let Ok(sh) = Shell::new() else {
        return false;
    };
    Command::from(cmd!(sh, "cargo nextest --version"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn selected_profile(explicit: Option<&str>) -> String {
    explicit.map(str::to_owned).unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|path| path.parent()?.file_name()?.to_str().map(str::to_owned))
            .filter(|name| name != "deps")
            .map(|name| if name == "debug" { "dev".into() } else { name })
            .unwrap_or_else(|| "dev".into())
    })
}

fn target_directory(root: &Path) -> Result<PathBuf> {
    let sh = Shell::new()?;
    sh.change_dir(root);
    let output = cmd!(sh, "cargo metadata --no-deps --format-version 1")
        .ignore_status()
        .output()?;
    ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(metadata["target_directory"]
        .as_str()
        .context("missing target directory")?
        .into())
}

fn build(root: &Path, profile: Option<&str>, torture: bool) -> Result<Built> {
    let profile = selected_profile(profile);
    ensure!(
        !torture || profile == "ci",
        "GCC torture requires --profile ci, matching the pinned baseline"
    );
    let mut binaries: BTreeSet<String> = tir_lit::required_binaries(root)?.into_values().collect();
    if torture {
        binaries.insert("fcc".into());
    }
    // Tools and tests share one build graph, so the runner's own build is a no-op.
    let sh = Shell::new()?;
    sh.change_dir(root);
    let status = Command::from(cmd!(
        sh,
        "cargo build --locked --workspace --bins --tests --examples --profile {profile}"
    ))
    .status()?;
    ensure!(status.success(), "Cargo test build failed");
    let binary_directory = target_directory(root)?.join(match profile.as_str() {
        "dev" => "debug",
        other => other,
    });
    for name in &binaries {
        let path = binary_directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        ensure!(
            path.is_file(),
            "required test tool was not built: {}",
            path.display()
        );
    }
    let corpus = torture
        .then(|| crate::fcc_torture::fetch_corpus(&Shell::new()?, root))
        .transpose()?;
    Ok(Built {
        profile,
        binaries,
        binary_directory,
        corpus,
    })
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
