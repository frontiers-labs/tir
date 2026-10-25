mod report;

use std::{fs, path::Path, process::Command};

use anyhow::{ensure, Context, Result};
use clap::Args;

#[derive(Args)]
pub struct Options {
    /// Cargo profile for the compiler and benchmarks.
    #[arg(long, default_value = "bench")]
    pub profile: String,
    /// Arguments passed to cargo bench, after `--`.
    #[arg(last = true)]
    pub cargo_args: Vec<String>,
}

pub fn run(root: &Path, options: Options) -> Result<()> {
    let metadata = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    ensure!(metadata.status.success(), "cargo metadata failed");
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let target = Path::new(
        metadata["target_directory"]
            .as_str()
            .context("missing target directory")?,
    );
    let results = std::env::var_os("CRITERION_HOME")
        .map(|path| root.join(path))
        .unwrap_or_else(|| target.join("criterion"));
    fs::create_dir_all(&results)?;

    let status = Command::new("cargo")
        .current_dir(root)
        .args([
            "build",
            "-p",
            "fcc",
            "--bin",
            "fcc",
            "--locked",
            "--profile",
        ])
        .arg(&options.profile)
        .status()?;
    ensure!(status.success(), "benchmark compiler build failed");

    let binaries = target.join(match options.profile.as_str() {
        "bench" | "release" => "release",
        "dev" | "test" => "debug",
        profile => profile,
    });
    let path = std::env::join_paths(std::iter::once(binaries).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))?;
    let previous = report::snapshot(&results)?;
    let status = Command::new("cargo")
        .current_dir(root)
        .args(["bench", "--locked", "--profile"])
        .arg(&options.profile)
        .args(options.cargo_args)
        .env("PATH", path)
        .env("CARGO_TARGET_DIR", target)
        .env("CRITERION_HOME", &results)
        .status()?;
    let csv = target.join("benchmarks.csv");
    report::write(&results, &previous, &csv, &mut std::io::stdout().lock())?;
    println!("Benchmark metrics saved to {}", csv.display());
    ensure!(status.success(), "benchmark run did not succeed");
    Ok(())
}
