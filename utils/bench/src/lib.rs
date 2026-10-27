//! Repeated command timing with Bencher Metric Format JSON output.

use criterion::measurement::{Measurement, ValueFormatter, WallTime};
pub use criterion::{BatchSize, BenchmarkId, Throughput, criterion_main};
use libprof::{CountingDriver, CountingDriverBuilder, MetricExpression};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

/// Criterion configured for wall-clock timing and TMA collection.
pub type Criterion = criterion::Criterion<TMAMetrics>;
/// Wall-clock measurement sharing TMA results across clones, including warmup.
#[derive(Clone)]
pub struct TMAMetrics(Rc<RefCell<State>>);
struct State {
    driver: Option<Box<dyn CountingDriver>>,
    values: Option<HashMap<String, f64>>,
}
impl TMAMetrics {
    /// Opens host TMA counters; unsupported formulas or cluster counters report unavailable.
    pub fn new() -> Self {
        let events = libprof::host_tma_scenario().map_or(Vec::new(), |s| s.events);
        let counters: Vec<_> = events.iter().map(|e| libprof::tma_counter(e)).collect();
        let builder = CountingDriverBuilder::new().counters(&counters);
        Self(Rc::new(RefCell::new(State {
            driver: builder.build().ok(),
            values: None,
        })))
    }
    /// Saves evaluated TMA percentages for the benchmark, then clears its counters.
    pub fn report(&self, name: &str) {
        let mut state = self.0.borrow_mut();
        let Some(mut values) = state.values.take() else {
            return;
        };
        let mut metrics = serde_json::Map::new();
        if let Some(scenario) = libprof::host_tma_scenario().filter(|_| state.driver.is_some()) {
            for constant in scenario.constants {
                values.insert(constant.name, constant.value as f64);
            }
            for metric in scenario.metrics {
                let Ok(value) = MetricExpression(metric.formula.replace('$', "")).evaluate(&values)
                else {
                    metrics.clear();
                    break;
                };
                values.insert(metric.name.clone(), value);
                metrics.insert(
                    format!("tma.{} (%)", metric.name),
                    serde_json::json!({"value": value * 100.0}),
                );
            }
        }
        if metrics.is_empty() {
            metrics.insert("tma.unavailable".into(), serde_json::json!({"value": 1}));
        }
        let _ = write_metrics(name, serde_json::Value::Object(metrics));
    }
}
impl Default for TMAMetrics {
    fn default() -> Self {
        Self::new()
    }
}
impl Measurement for TMAMetrics {
    type Intermediate = (Instant, bool);
    type Value = Duration;
    fn start(&self) -> Self::Intermediate {
        let mut state = self.0.borrow_mut();
        let driver = state.driver.as_mut();
        let active = driver.is_some_and(|d| d.reset().and_then(|()| d.start()).is_ok());
        (Instant::now(), active)
    }
    fn end(&self, (started, active): Self::Intermediate) -> Duration {
        let elapsed = started.elapsed();
        let mut state = self.0.borrow_mut();
        let counts = state.driver.as_mut().and_then(|d| {
            d.stop().ok().filter(|_| active)?;
            d.counters()
                .ok()
                .filter(|r| !r.entries().is_empty() && r.entries().iter().all(|e| e.core.is_none()))
        });
        let values = state.values.get_or_insert_default();
        if let Some(counts) = counts {
            for entry in counts.entries() {
                let name = entry.counter.name().to_owned();
                *values.entry(name).or_default() += entry.value.value as f64;
            }
        } else {
            state.driver = None;
        }
        elapsed
    }
    fn add(&self, a: &Duration, b: &Duration) -> Duration {
        WallTime.add(a, b)
    }
    fn zero(&self) -> Duration {
        WallTime.zero()
    }
    fn to_f64(&self, value: &Duration) -> f64 {
        WallTime.to_f64(value)
    }
    fn formatter(&self) -> &dyn ValueFormatter {
        WallTime.formatter()
    }
}

/// Defines a benchmark group with shared TMA collection and explicit reports.
#[macro_export]
macro_rules! criterion_group {
    ($name:ident, $($bench:path),+ $(,)?) => {
        fn $name() {
            let tma = $crate::TMAMetrics::new();
            let mut c = ::criterion::Criterion::default()
                .with_measurement(tma.clone()).sample_size(10).configure_from_args();
            $($bench(&mut c, &tma);)+
        }
    };
}

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
    /// Benchmark a program, with defaults of 3 repetitions and 1 second.
    ///
    /// Arguments are passed literally. For shell syntax, use `sh` with
    /// `.args(["-c", "your shell command"])`.
    pub fn new(name: impl Into<String>, program: impl AsRef<OsStr>) -> Self {
        Self {
            name: name.into(),
            command: Command::new(program),
            min_repetitions: 3,
            min_duration: Duration::from_secs(1),
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
    pub fn run_validated(self, validate: impl FnOnce(&str)) -> io::Result<String> {
        self.measure(|duration, _| Ok(duration), validate)
    }

    /// Run the experiment, taking each sample from the log of its repetition
    /// instead of the wall clock, for programs such as CoreMark that time
    /// their own workload. A sample error ends the experiment without saving
    /// results.
    pub fn run_self_timed(
        self,
        sample: impl Fn(&str) -> io::Result<Duration>,
    ) -> io::Result<String> {
        self.measure(|_, output| sample(output), |_| {})
    }

    fn measure(
        mut self,
        sample: impl Fn(Duration, &str) -> io::Result<Duration>,
        validate: impl FnOnce(&str),
    ) -> io::Result<String> {
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
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            log.push_str(&text);
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "benchmark {:?} command {:?} failed with {}\n{log}",
                    self.name, self.command, output.status,
                )));
            }
            elapsed += duration;
            samples.push(sample(duration, &text)?.as_secs_f64() * 1e9);
        }
        validate(&log);
        let metrics = serde_json::json!({
            "latency": {
                "value": samples.iter().sum::<f64>() / samples.len() as f64,
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
        assert!(total >= 1e9);
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
