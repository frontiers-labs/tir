//! Cargo benchmark harness for Rust functions and verified external programs.
//! Benchmark definitions are Rust code; native and Cachegrind runs share cases.
pub mod environment;
mod function;
mod options;
pub mod process;
pub mod program;
mod results;
pub mod sources;

pub use anyhow::Result;
pub use function::Bencher;
pub use options::{DEFAULT_TIMEOUT_SECS, Engine, Options, Phase};
pub use process::Command;

use anyhow::{Context, ensure};
use clap::Parser;
use globset::{Glob, GlobMatcher};
use results::{Metrics, Record, Results};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A check of captured stdout, executed after the measured operation.
pub type Validator = Box<dyn Fn(&Path) -> Result<()>>;

/// A subprocess and its untimed output validator. The validator receives stdout.
pub struct ProcessCase {
    pub id: String,
    pub command: Command,
    pub verify: Option<Validator>,
    /// Workload identity and measurement scope. Must be stable between comparisons.
    pub metadata: Value,
    /// Whether this case is subject to regression thresholds.
    pub gate: bool,
}

/// One Cargo benchmark target. All measured work runs serially under a host lock.
pub struct Suite {
    options: Options,
    filter: GlobMatcher,
    directory: PathBuf,
    target: PathBuf,
    results: Results,
    seen: BTreeSet<String>,
    throughput: Option<u64>,
    contract_version: u32,
    _environment: Option<environment::EnvironmentGuard>,
}

impl Suite {
    /// Parse the common Cargo harness arguments and open a result bundle.
    pub fn from_args(namespace: &str) -> Result<Self> {
        Self::new(namespace, Options::parse())
    }

    /// Create a harness with explicit options. Listing and profiling workers do
    /// not acquire a second host lock or create another result bundle.
    pub fn new(namespace: &str, mut options: Options) -> Result<Self> {
        ensure!(
            options.threshold.is_finite()
                && options.threshold >= 0.0
                && options.rss_threshold.is_finite()
                && options.rss_threshold >= 0.0,
            "thresholds must be finite and nonnegative"
        );
        ensure!(
            options.samples > 0 && options.timeout > 0 && options.sample_time_ms > 0,
            "samples, sample duration and timeout must be positive"
        );
        let filter = Glob::new(&options.filter)?.compile_matcher();
        let (target, workspace) = cargo_directories(Duration::from_secs(options.timeout))?;
        for path in [&mut options.output, &mut options.baseline]
            .into_iter()
            .flatten()
        {
            if path.is_relative() {
                *path = workspace.join(&*path);
            }
        }
        let active = !options.list && options.worker.is_none();
        let directory = if active {
            let parent = options
                .output
                .clone()
                .unwrap_or_else(|| target.join("bench"));
            std::fs::create_dir_all(&parent)?;
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = parent.join(format!(
                "{}-{stamp}-{}",
                namespace.replace('/', "-"),
                std::process::id()
            ));
            std::fs::create_dir(&path)?;
            path.canonicalize()?
        } else {
            PathBuf::new()
        };
        let guard = if active {
            Some(environment::EnvironmentGuard::acquire(
                &std::env::temp_dir().join(format!("tir-bench-{}.lock", unsafe { libc::getuid() })),
                options.cpu,
            )?)
        } else {
            None
        };
        let mut environment = guard
            .as_ref()
            .map(|g| serde_json::to_value(&g.metadata))
            .transpose()?
            .unwrap_or(Value::Null);
        if active && options.engine == Engine::Cachegrind {
            let version = process::probe_cachegrind(
                &directory.join("valgrind-version"),
                Duration::from_secs(options.timeout),
            )?;
            environment["valgrind"] = json!(version);
        }
        if active {
            environment["build"] = build_configuration();
            environment["native_process_accounting"] = json!({
                "wall_and_cpu_scope":"GNU time supervisor plus command",
                "rss_scope":"command process high-water, GNU time %M in KiB normalized to bytes",
                "supervisor_version":process::capture(std::process::Command::new("/usr/bin/time").arg("--version"), Duration::from_secs(options.timeout)).ok().filter(|o|o.status.success()).map(|o|String::from_utf8_lossy(&o.stdout).into_owned())
            });
        }
        let results = Results {
            schema: 3,
            namespace: namespace.into(),
            engine: options.engine,
            environment,
            provenance: if active {
                provenance(Duration::from_secs(options.timeout))
            } else {
                Value::Null
            },
            status: "incomplete".into(),
            cases: Vec::new(),
        };
        if active {
            results.save(&directory)?;
        }
        Ok(Self {
            options,
            filter,
            directory,
            target,
            results,
            seen: BTreeSet::new(),
            throughput: None,
            contract_version: 1,
            _environment: guard,
        })
    }

    pub fn options(&self) -> &Options {
        &self.options
    }
    /// Untimed preparation and measured output are retained under this directory.
    pub fn artifacts(&self) -> &Path {
        &self.directory
    }
    pub fn source_cache(&self) -> PathBuf {
        source_cache(&self.target)
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.options.timeout)
    }

    /// Match a full process ID or a function name relative to this target.
    pub fn matches(&self, name: &str) -> bool {
        let prefix = format!("{}/", self.results.namespace);
        let relative = name.strip_prefix(&prefix).unwrap_or(name);
        let qualified = format!("{prefix}{relative}");
        if let Some(worker) = &self.options.worker {
            return relative == worker || qualified == *worker;
        }
        self.filter.is_match(relative) || self.filter.is_match(qualified)
    }

    pub fn list_case(&mut self, name: &str) -> Result<()> {
        if self.matches(name) && self.options.list {
            ensure!(
                self.seen.insert(name.into()),
                "duplicate benchmark ID {name}"
            );
            println!("{name}");
        }
        Ok(())
    }

    pub fn list_function(&mut self, name: &str) -> Result<()> {
        if self.options.list && self.matches(name) {
            let id = format!("{}/{}", self.results.namespace, name);
            ensure!(self.seen.insert(id.clone()), "duplicate benchmark ID {id}");
            println!("{id}");
        }
        Ok(())
    }

    /// Version of the workload's inputs and setup semantics. Increment when those
    /// change so earlier measurements cannot pass compatibility checks silently.
    pub fn set_contract_version(&mut self, version: u32) {
        self.contract_version = version;
    }

    /// Bytes processed by subsequent function cases, for bytes/second reporting.
    pub fn set_throughput(&mut self, bytes: u64) {
        self.throughput = Some(bytes);
    }

    /// Measure a function, excluding code outside the bencher operation. The
    /// callback must call `iter` or `iter_batched` exactly once per invocation.
    pub fn function(&mut self, name: &str, mut operation: impl FnMut(&mut Bencher)) -> Result<()> {
        if !self.matches(name) {
            return Ok(());
        }
        let id = format!("{}/{}", self.results.namespace, name);
        if self.options.list {
            return self.list_function(name);
        }
        ensure!(!self.seen.contains(&id), "duplicate benchmark ID {id}");
        if self.options.worker.is_some() || self.options.engine == Engine::Cachegrind {
            ensure!(
                function::header_version().is_some_and(|version| version >= (3, 22)),
                "function profiling requires --features tir-bench/cachegrind and Valgrind 3.22+ headers; rebuild valgrind-requests after installing headers, or set VALGRIND_REQUESTS_VALGRIND_INCLUDE"
            );
        }
        if self.options.worker.is_some() {
            let mut bencher = Bencher::new(self.options.iterations.max(1), true);
            operation(&mut bencher);
            ensure!(
                bencher.calls == 1,
                "benchmark must call iter or iter_batched exactly once"
            );
            self.seen.insert(id);
            return Ok(());
        }
        if self.options.engine == Engine::Cachegrind {
            let command = Command::new(std::env::current_exe()?)
                .args([
                    "--worker",
                    &id,
                    "--iterations",
                    &self.options.iterations.max(1).to_string(),
                ])
                .cachegrind_regions(true);
            return self.process_group(vec![ProcessCase { id, command, verify: None, metadata: json!({"scope":"function-region", "contract":self.contract_version, "iterations":self.options.iterations.max(1)}), gate: true }]);
        }
        let mut iterations = self.options.iterations.max(1);
        if self.options.iterations == 0 {
            iterations = function::calibrate(
                Duration::from_millis(self.options.sample_time_ms),
                |count| {
                    let mut calibration = Bencher::new(count, false);
                    operation(&mut calibration);
                    ensure!(
                        calibration.calls == 1,
                        "benchmark must call iter or iter_batched exactly once"
                    );
                    Ok(calibration.elapsed)
                },
            )?;
        }
        let mut samples = Vec::new();
        for index in 0..u64::from(self.options.warmups) + u64::from(self.options.samples) {
            let mut bencher = Bencher::new(iterations, false);
            operation(&mut bencher);
            ensure!(
                bencher.calls == 1,
                "benchmark must call iter or iter_batched exactly once"
            );
            if index < u64::from(self.options.warmups) {
                continue;
            }
            let latency = bencher.elapsed.as_secs_f64() * 1e9 / iterations as f64;
            let mut metrics = Metrics::from([
                ("latency".into(), latency),
                (
                    "batch_elapsed_ns".into(),
                    bencher.elapsed.as_secs_f64() * 1e9,
                ),
                ("iterations".into(), iterations as f64),
            ]);
            if let Some(bytes) = self.throughput.filter(|_| latency > 0.0) {
                metrics.insert(
                    "throughput_bytes_per_second".into(),
                    bytes as f64 * 1e9 / latency,
                );
            }
            samples.push(metrics);
        }
        self.record(id, json!({"scope":"function", "contract":self.contract_version, "bytes_per_iteration":self.throughput, "fixed_iterations":self.options.iterations,"sample_time_ms":self.options.sample_time_ms}), true, samples)
    }

    /// Interleave variants in rotated order, validating every successful execution
    /// after measurement. Preparation must have completed before calling this.
    pub fn process_group(&mut self, cases: Vec<ProcessCase>) -> Result<()> {
        let cases: Vec<_> = cases.into_iter().filter(|c| self.matches(&c.id)).collect();
        if self.options.list {
            for case in cases {
                self.list_case(&case.id)?;
            }
            return Ok(());
        }
        let mut group_ids = BTreeSet::new();
        for case in &cases {
            ensure!(
                !self.seen.contains(&case.id) && group_ids.insert(&case.id),
                "duplicate benchmark ID {}",
                case.id
            );
        }
        let mut samples = vec![Vec::new(); cases.len()];
        let rounds = if self.options.engine == Engine::Cachegrind {
            1
        } else {
            self.options.samples
        };
        let warmups = if self.options.engine == Engine::Cachegrind {
            0
        } else {
            self.options.warmups
        };
        for round in 0..u64::from(rounds) + u64::from(warmups) {
            for offset in 0..cases.len() {
                let index = (round as usize + offset) % cases.len();
                let case = &cases[index];
                let dir = self
                    .directory
                    .join("samples")
                    .join(safe_id(&case.id))
                    .join(round.to_string());
                let sample = case
                    .command
                    .execute(&dir, self.timeout(), self.options.engine)
                    .with_context(|| format!("benchmark {}", case.id))?;
                if let Some(verify) = &case.verify {
                    verify(&sample.stdout).with_context(|| format!("validator for {}", case.id))?;
                }
                if round < u64::from(warmups) {
                    continue;
                }
                let metrics = if self.options.engine == Engine::Cachegrind {
                    ensure!(
                        sample.counters.get("Ir").is_some_and(|v| *v > 0.0),
                        "Cachegrind did not observe instructions for {}",
                        case.id
                    );
                    sample.counters
                } else {
                    Metrics::from([
                        ("latency".into(), sample.wall_ns),
                        ("user_cpu_ns".into(), sample.user_ns),
                        ("system_cpu_ns".into(), sample.system_ns),
                        (
                            "peak_process_rss_bytes".into(),
                            sample.peak_process_rss_bytes as f64,
                        ),
                    ])
                };
                samples[index].push(metrics);
            }
        }
        for (case, samples) in cases.into_iter().zip(samples) {
            let metadata =
                json!({"workload":case.metadata,"trace_children":case.command.trace_children});
            self.record(case.id, metadata, case.gate, samples)?;
        }
        Ok(())
    }

    fn record(
        &mut self,
        id: String,
        metadata: Value,
        gate: bool,
        samples: Vec<Metrics>,
    ) -> Result<()> {
        ensure!(self.seen.insert(id.clone()), "duplicate benchmark ID {id}");
        let summary = results::summarize(&samples)?;
        eprintln!("{id}: {}", serde_json::to_string(&summary)?);
        self.results.cases.push(Record {
            id,
            metadata,
            gate,
            samples,
            summary,
        });
        self.results.save(&self.directory)
    }

    /// Check coverage, baseline compatibility and host policy before marking the
    /// result complete. Earlier failures leave an unusable baseline with logs.
    pub fn finish(mut self) -> Result<()> {
        if self.options.list {
            return Ok(());
        }
        if self.options.worker.is_some() {
            ensure!(
                self.seen.len() == 1,
                "profiling worker did not execute exactly one case"
            );
            return Ok(());
        }
        ensure!(
            self.results.cases.len() >= self.options.min_cases,
            "expected at least {} cases, measured {}",
            self.options.min_cases,
            self.results.cases.len()
        );
        self.results.cases.sort_by(|a, b| a.id.cmp(&b.id));
        if let Some(path) = &self.options.baseline {
            let path = if path.is_dir() {
                path.join("results.json")
            } else {
                path.clone()
            };
            let baseline = serde_json::from_slice(&std::fs::read(path)?)?;
            self.results.compare(&baseline, &self.options)?;
        }
        if let Some(environment) = &self._environment {
            environment.check_stable()?;
        }
        self.results.status = "complete".into();
        self.results.save(&self.directory)?;
        eprintln!("Benchmark artifacts: {}", self.directory.display());
        Ok(())
    }
}

fn safe_id(id: &str) -> String {
    // Preserve readable names, with a digest to prevent collisions and traversal.
    use sha2::{Digest, Sha256};
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    format!("{clean}-{:x}", Sha256::digest(id.as_bytes()))
}

fn cargo_directories(timeout: Duration) -> Result<(PathBuf, PathBuf)> {
    let output = process::capture(
        std::process::Command::new("cargo").args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ]),
        timeout,
    )?;
    ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    Ok((
        PathBuf::from(
            metadata["target_directory"]
                .as_str()
                .context("Cargo target directory missing")?,
        ),
        PathBuf::from(
            metadata["workspace_root"]
                .as_str()
                .context("Cargo workspace root missing")?,
        ),
    ))
}

fn provenance(timeout: Duration) -> Value {
    let git = |args: &[&str]| {
        process::capture(std::process::Command::new("git").args(args), timeout)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    json!({"revision":git(&["rev-parse","HEAD"]),"dirty":git(&["status","--porcelain"]),"executable":std::env::current_exe().ok(),"executable_sha256":executable_digest(),"arguments":std::env::args().collect::<Vec<_>>()})
}

/// Build settings of the harness and its Cargo-built compiler dependencies.
pub fn build_configuration() -> Value {
    json!({"profile":env!("TIR_BENCH_PROFILE"), "opt_level":env!("TIR_BENCH_OPT_LEVEL"), "debug":env!("TIR_BENCH_DEBUG"), "target":env!("TIR_BENCH_TARGET"), "rustflags":env!("TIR_BENCH_CARGO_ENCODED_RUSTFLAGS"), "rustc":env!("TIR_BENCH_RUSTC"), "valgrind_headers":function::header_version()})
}

fn executable_digest() -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        hash.input(&buffer[..count]);
    }
    Some(format!("{:x}", hash.result()))
}

/// Resolve the shared pinned-source cache, honouring an explicit environment override.
pub fn source_cache(target: &Path) -> PathBuf {
    std::env::var_os("TIR_BENCH_SOURCE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| target.join("bench-sources"))
}
