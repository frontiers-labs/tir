//! Repeated command timing with Bencher Metric Format JSON output.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A command timed until both the repetition count and duration are satisfied.
///
/// ```no_run
/// use tir_bench::Experiment;
///
/// let log = Experiment::new("compile", "gcc")
///     .args(["-c", "example.c", "-o", "example.o"])
///     .run()?;
/// println!("{log}");
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Experiment {
    name: String,
    command: Command,
    min_repetitions: usize,
    min_duration: Duration,
}

impl Experiment {
    /// Benchmark a program, with defaults of 3 repetitions and 5 seconds.
    ///
    /// Arguments are passed literally. For shell syntax, use `sh` with
    /// `.args(["-c", "your shell command"])`.
    pub fn new(name: impl Into<String>, program: impl AsRef<OsStr>) -> Self {
        Self {
            name: name.into(),
            command: Command::new(program),
            min_repetitions: 3,
            min_duration: Duration::from_secs(5),
        }
    }

    /// Append command-line arguments without shell expansion.
    pub fn args(mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.command.args(args);
        self
    }

    /// Set the minimum number of successful runs. Must be greater than zero.
    pub fn min_repetitions(mut self, repetitions: usize) -> Self {
        self.min_repetitions = repetitions;
        self
    }

    /// Set the minimum sum of measured command durations.
    pub fn min_duration(mut self, duration: Duration) -> Self {
        self.min_duration = duration;
        self
    }

    /// Run the experiment, save Bencher Metric Format JSON, and return its log.
    ///
    /// Results overwrite `<criterion directory>/<name>/new/bencher.json`.
    /// The directory follows Criterion: `CRITERION_HOME`, then the Cargo target
    /// directory plus `criterion`, then `target/criterion`. Filename separators
    /// and reserved characters in the benchmark name are replaced with `_`.
    ///
    /// `latency.value` is the arithmetic mean in nanoseconds. The extra
    /// `latency.samples` field preserves every duration in execution order,
    /// also in nanoseconds. Timing includes process startup and waiting for exit.
    /// Standard input is closed. The returned log concatenates stdout then
    /// stderr for each repetition, replacing invalid UTF-8 with U+FFFD.
    ///
    /// Returns an error for an empty name, zero repetitions, failure to start
    /// the command, an unsuccessful exit, or failure to save results. An
    /// unsuccessful exit includes the captured log and writes no results.
    pub fn run(self) -> io::Result<String> {
        self.run_validated(|_| {})
    }

    /// Run the experiment and validate its output before saving measurements.
    pub fn run_validated(mut self, validate: impl FnOnce(&str)) -> io::Result<String> {
        if self.name.is_empty() || self.min_repetitions == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "benchmark name must be nonempty and repetitions must be positive",
            ));
        }
        self.command.stdin(Stdio::null());
        let mut samples = Vec::new();
        let mut elapsed = Duration::ZERO;
        let mut log = String::new();
        while samples.len() < self.min_repetitions || elapsed < self.min_duration {
            let start = Instant::now();
            let output = self.command.output()?;
            let duration = start.elapsed();
            log.push_str(&String::from_utf8_lossy(&output.stdout));
            log.push_str(&String::from_utf8_lossy(&output.stderr));
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "benchmark {:?} command {:?} failed with {}\n{log}",
                    self.name, self.command, output.status,
                )));
            }
            elapsed += duration;
            samples.push(duration.as_secs_f64() * 1e9);
        }
        validate(&log);
        let metrics = serde_json::json!({
            "latency": {
                "value": elapsed.as_secs_f64() * 1e9 / samples.len() as f64,
                "samples": samples,
            }
        });
        write_metrics(&self.name, metrics)?;
        Ok(log)
    }

    /// Record a skipped experiment and its reason without running the command.
    pub fn skip(mut self, reason: &str) -> io::Result<()> {
        eprintln!("{}: skipped ({reason})", self.name);
        self.name = format!("{} [skipped: {reason}]", self.name);
        write_metrics(&self.name, serde_json::json!({"skipped": {"value": 1}}))
    }
}

fn write_metrics(name: &str, metrics: serde_json::Value) -> io::Result<()> {
    let mut directory = name.replace(['?', '"', '/', '\\', '*', '<', '>', ':', '|', '^'], "_");
    if directory == "." || directory == ".." {
        directory = "_".into();
    }
    let json = serde_json::json!({name: metrics});
    let directory = output_directory().join(directory).join("new");
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join("bencher.json"),
        serde_json::to_vec_pretty(&json)?,
    )
}

fn output_directory() -> PathBuf {
    if let Some(home) = std::env::var_os("CRITERION_HOME") {
        return home.into();
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let cargo = std::env::var_os("CARGO")?;
            let output = Command::new(cargo)
                .args(["metadata", "--format-version", "1", "--no-deps"])
                .output()
                .ok()?;
            let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
            metadata["target_directory"].as_str().map(PathBuf::from)
        });
    target
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("criterion")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn read_result(directory: &str) -> serde_json::Value {
        let bytes = fs::read(output_directory().join(directory).join("new/bencher.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn defaults_meet_both_minimums() {
        let log = Experiment::new("tir-bench-test-sleep", "sh")
            .args(["-c", "sleep 0.05"])
            .run()
            .unwrap();
        assert!(log.is_empty());
        let json = read_result("tir-bench-test-sleep");
        let latency = &json["tir-bench-test-sleep"]["latency"];
        let samples = latency["samples"].as_array().unwrap();
        assert!(samples.len() >= 3);
        let total: f64 = samples.iter().map(|s| s.as_f64().unwrap()).sum();
        assert!(total >= 5e9);
        let mean = latency["value"].as_f64().unwrap();
        assert!((mean - total / samples.len() as f64).abs() < 1.0);
    }

    #[test]
    fn repetitions_and_literal_arguments() {
        for repetitions in [None, Some(4)] {
            let mut experiment = Experiment::new("tir-bench-test/quoted \"name\"", "sh")
                .args([
                    "-c",
                    "test \"$1\" = 'a b;$HOME' && printf 'out\\n' && printf 'err\\n' >&2",
                    "sh",
                    "a b;$HOME",
                ])
                .min_duration(Duration::ZERO);
            if let Some(count) = repetitions {
                experiment = experiment.min_repetitions(count);
            }
            let log = experiment.run().unwrap();
            assert_eq!(log, "out\nerr\n".repeat(repetitions.unwrap_or(3)));
            let json = read_result("tir-bench-test_quoted _name_");
            assert_eq!(
                json["tir-bench-test/quoted \"name\""]["latency"]["samples"]
                    .as_array()
                    .unwrap()
                    .len(),
                repetitions.unwrap_or(3)
            );
        }
    }

    #[test]
    fn failures_do_not_become_measurements() {
        let directory = output_directory().join("tir-bench-test-rejected-output");
        let _ = fs::remove_dir_all(&directory);
        let rejected = std::panic::catch_unwind(|| {
            Experiment::new("tir-bench-test-rejected-output", "sh")
                .args(["-c", "printf incorrect"])
                .min_duration(Duration::ZERO)
                .min_repetitions(1)
                .run_validated(|log| assert_eq!(log, "correct"))
                .unwrap();
        });
        assert!(rejected.is_err());
        assert!(!directory.exists());
        Experiment::new("tir-bench-test-skipped", "/tir-bench-nonexistent-command")
            .skip("unsupported")
            .unwrap();
        let skipped = read_result("tir-bench-test-skipped [skipped_ unsupported]");
        let status = &skipped["tir-bench-test-skipped [skipped: unsupported]"];
        assert_eq!(status["skipped"]["value"], 1);
        assert!(status.get("latency").is_none());
        let error = Experiment::new("failure", "sh")
            .args(["-c", "echo failed-command >&2; exit 7"])
            .run()
            .unwrap_err();
        assert!(error.to_string().contains("exit status: 7"));
        assert!(error.to_string().contains("failed-command"));
        assert_eq!(
            Experiment::new("missing", "/tir-bench-nonexistent-command")
                .run()
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        for experiment in [
            Experiment::new("", "sh"),
            Experiment::new("zero", "sh").min_repetitions(0),
        ] {
            assert_eq!(
                experiment.run().unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
