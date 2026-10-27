mod report;

use std::{fs, path::Path, process::Command};

use anyhow::{ensure, Context, Result};
use clap::Args;

#[derive(Args)]
pub struct Options {
    /// Cargo profile for the compiler and benchmarks.
    #[arg(long, default_value = "bench")]
    pub profile: String,
    /// Arguments passed to cargo nextest bench or cargo bench, after `--`.
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

    let previous = report::snapshot(&results)?;
    let mut runner = Command::new("cargo");
    if crate::test::has_nextest() {
        // nextest's default profile leaves out the comparison baselines; CI
        // selects its perf profile to run them.
        runner.args(["nextest", "bench", "--no-tests", "warn", "--cargo-profile"]);
    } else {
        eprintln!("cargo-nextest is not installed; running cargo bench with the comparisons");
        runner.args(["bench", "--profile"]);
    }
    let status = runner
        .current_dir(root)
        .arg(&options.profile)
        .arg("--locked")
        .args(options.cargo_args)
        .env("CARGO_TARGET_DIR", target)
        .env("CRITERION_HOME", &results)
        .status()?;
    let csv = target.join("benchmarks.csv");
    report::write(&results, &previous, &csv, &mut std::io::stdout().lock())?;
    println!("Benchmark metrics saved to {}", csv.display());
    ensure!(status.success(), "benchmark run did not succeed");
    Ok(())
}
