//! State shared by every benchmark of a target: options, the host lock, the
//! result bundle, and the measurement of processes and counted functions.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::results::{self, Metrics, Record, Results, Variant};
use crate::{
    Command, Engine, Measure, Options, ProcessCase, build_configuration, environment, options,
    process, program, source_cache,
};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    List,
    /// Native engine: Criterion times functions and processes are sampled.
    Time,
    /// Cachegrind engine: processes run once and functions are queued for children.
    Count,
    /// A child of the Cachegrind engine that executes one function benchmark.
    CountChild,
}

enum Filter {
    All,
    Regex(regex::Regex),
    Exact(String),
}

pub(crate) struct Harness {
    pub(crate) options: Options,
    pub(crate) mode: Mode,
    filter: Filter,
    /// The result bundle. Created on first use so native function runs leave none.
    directory: Option<PathBuf>,
    target: PathBuf,
    pub(crate) workspace: PathBuf,
    pub(crate) compilers: Option<program::CompilerSet>,
    results: Results,
    seen: BTreeSet<String>,
    /// Function benchmarks awaiting their Cachegrind child.
    pub(crate) functions: Vec<String>,
    pub(crate) counted: bool,
    pub(crate) failure: Option<anyhow::Error>,
    environment: Option<environment::EnvironmentGuard>,
}

impl Harness {
    pub(crate) fn new(namespace: &str, options: Options) -> Result<Self> {
        ensure!(
            options.threshold.is_finite()
                && options.threshold >= 0.0
                && options.rss_threshold.is_finite()
                && options.rss_threshold >= 0.0,
            "thresholds must be finite and nonnegative"
        );
        let filter = match &options.filter {
            None => Filter::All,
            Some(filter) if options.exact => Filter::Exact(filter.clone()),
            Some(filter) => Filter::Regex(regex::Regex::new(filter)?),
        };
        let mode = if options.count_function.is_some() {
            Mode::CountChild
        } else if options.list {
            Mode::List
        } else if options.engine == Engine::Cachegrind {
            Mode::Count
        } else {
            Mode::Time
        };
        let mut harness = Self {
            mode,
            filter,
            directory: None,
            target: PathBuf::new(),
            workspace: PathBuf::new(),
            compilers: None,
            results: Results {
                schema: 3,
                namespace: namespace.into(),
                engine: options.engine,
                environment: Value::Null,
                provenance: Value::Null,
                status: "incomplete".into(),
                cases: Vec::new(),
            },
            seen: BTreeSet::new(),
            functions: Vec::new(),
            counted: false,
            failure: None,
            environment: None,
            options,
        };
        // The parent holds the host lock and owns the bundle.
        if harness.mode == Mode::CountChild {
            return Ok(harness);
        }
        let timeout = harness.timeout();
        (harness.target, harness.workspace) = cargo_directories(timeout)?;
        for path in [&mut harness.options.output, &mut harness.options.baseline]
            .into_iter()
            .flatten()
        {
            if path.is_relative() {
                *path = harness.workspace.join(&*path);
            }
        }
        if harness.mode == Mode::List {
            return Ok(harness);
        }
        let guard = environment::EnvironmentGuard::acquire(
            &std::env::temp_dir().join(format!("tir-bench-{}.lock", unsafe { libc::getuid() })),
            harness.options.cpu,
        )?;
        let mut environment = serde_json::to_value(&guard.metadata)?;
        environment["build"] = build_configuration();
        let supervisor_version = process::capture(
            std::process::Command::new("/usr/bin/time").arg("--version"),
            timeout,
        )
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
        environment["native_process_accounting"] = json!({
            "wall_and_cpu_scope": "GNU time supervisor plus command",
            "rss_scope": "command process high-water, GNU time %M in KiB normalized to bytes",
            "supervisor_version": supervisor_version,
        });
        harness.results.environment = environment;
        harness.results.provenance = provenance(timeout);
        harness.environment = Some(guard);
        Ok(harness)
    }

    /// Untimed preparation and measured output are retained under this directory.
    pub(crate) fn bundle(&mut self) -> Result<PathBuf> {
        if let Some(directory) = &self.directory {
            return Ok(directory.clone());
        }
        let parent = self
            .options
            .output
            .clone()
            .unwrap_or_else(|| self.target.join("bench"));
        std::fs::create_dir_all(&parent)?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = parent.join(format!(
            "{}-{stamp}-{}",
            self.results.namespace.replace('/', "-"),
            std::process::id()
        ));
        std::fs::create_dir(&path)?;
        let directory = path.canonicalize()?;
        if self.options.engine == Engine::Cachegrind {
            let version =
                process::probe_cachegrind(&directory.join("valgrind-version"), self.timeout())?;
            self.results.environment["valgrind"] = json!(version);
        }
        self.results.save(&directory)?;
        self.directory = Some(directory.clone());
        Ok(directory)
    }

    pub(crate) fn listing(&self) -> bool {
        self.mode == Mode::List
    }
    /// Programs are skipped on request and inside a function-counting child.
    pub(crate) fn skips_programs(&self) -> bool {
        self.options.skip_programs || self.mode == Mode::CountChild
    }
    pub(crate) fn source_cache(&self) -> PathBuf {
        source_cache(&self.target)
    }
    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_secs(self.options.timeout)
    }

    pub(crate) fn matches(&self, id: &str) -> bool {
        match (&self.options.count_function, &self.filter) {
            (Some(function), _) => function == id,
            (None, Filter::All) => true,
            (None, Filter::Regex(regex)) => regex.is_match(id),
            (None, Filter::Exact(exact)) => exact == id,
        }
    }

    /// Decide what a function benchmark does in this mode. `None` means its
    /// routine does not run now: it was filtered out, listed, or queued for a child.
    pub(crate) fn function(&mut self, id: &str) -> Option<Measure<()>> {
        if self.failure.is_some() || !self.matches(id) {
            return None;
        }
        match self.mode {
            Mode::List => self.attempt(|harness| harness.list_case(id)),
            Mode::Count => self.functions.push(id.into()),
            Mode::Time => return Some(Measure::Time(())),
            Mode::CountChild => {
                self.counted = true;
                return Some(Measure::Count);
            }
        }
        None
    }

    pub(crate) fn list_case(&mut self, id: &str) -> Result<()> {
        ensure!(self.seen.insert(id.into()), "duplicate benchmark ID {id}");
        println!("{id}: benchmark");
        Ok(())
    }

    /// Run `work` unless an earlier benchmark failed, and keep its failure for `finish`.
    pub(crate) fn attempt(&mut self, work: impl FnOnce(&mut Self) -> Result<()>) {
        if self.failure.is_none()
            && let Err(failure) = work(self)
        {
            self.failure = Some(failure);
        }
    }

    /// Interleave variants in rotated order, validating every successful execution
    /// after measurement. Preparation must have completed before calling this.
    pub(crate) fn process_group(&mut self, cases: Vec<ProcessCase>) -> Result<()> {
        let cases: Vec<_> = cases.into_iter().filter(|c| self.matches(&c.id)).collect();
        if self.listing() {
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
        let counting = self.options.engine == Engine::Cachegrind;
        let (rounds, warmups) = if counting {
            (1, 0)
        } else {
            let samples = self
                .options
                .sample_size
                .unwrap_or(options::DEFAULT_PROCESS_SAMPLES);
            (u64::from(samples), u64::from(self.options.warmups))
        };
        let directory = self.bundle()?;
        let mut samples = vec![Vec::new(); cases.len()];
        for round in 0..rounds + warmups {
            for offset in 0..cases.len() {
                let index = (round as usize + offset) % cases.len();
                let case = &cases[index];
                let dir = directory
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
                if round < warmups {
                    continue;
                }
                samples[index].push(if counting {
                    counted(&case.id, sample.counters)?
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
                });
            }
        }
        for (case, samples) in cases.into_iter().zip(samples) {
            let metadata =
                json!({"workload":case.metadata,"trace_children":case.command.trace_children});
            self.record(case.id, metadata, case.gate, case.variant, samples)?;
        }
        Ok(())
    }

    /// Run each queued function benchmark once in its own Cachegrind child. The
    /// child counts only the region its `Bencher` marks.
    pub(crate) fn count_functions(&mut self) -> Result<()> {
        let functions = std::mem::take(&mut self.functions);
        if functions.is_empty() {
            return Ok(());
        }
        let executable = std::env::current_exe()?;
        let directory = self.bundle()?;
        for id in functions {
            let sample = Command::new(&executable)
                .args(["--bench", "--count-function", &id])
                .current_dir(std::env::current_dir()?)
                .instrument_at_start(false)
                .execute(
                    &directory.join("samples").join(safe_id(&id)),
                    self.timeout(),
                    Engine::Cachegrind,
                )
                .with_context(|| format!("benchmark {id}"))?;
            let counters = counted(&id, sample.counters)?;
            self.record(id, json!({"scope": "function"}), true, None, vec![counters])?;
        }
        Ok(())
    }

    fn record(
        &mut self,
        id: String,
        metadata: Value,
        gate: bool,
        variant: Option<Variant>,
        samples: Vec<Metrics>,
    ) -> Result<()> {
        ensure!(self.seen.insert(id.clone()), "duplicate benchmark ID {id}");
        let summary = results::summarize(&samples)?;
        results::report(&id, &summary, samples.len());
        self.results.cases.push(Record {
            id,
            metadata,
            gate,
            variant,
            samples,
            summary,
        });
        let directory = self.bundle()?;
        self.results.save(&directory)
    }

    /// Check coverage, baseline compatibility and host policy before marking the
    /// result complete. Earlier failures leave an unusable baseline with logs.
    pub(crate) fn finish(mut self) -> Result<()> {
        match self.mode {
            Mode::List => return Ok(()),
            Mode::CountChild => {
                ensure!(
                    self.counted,
                    "function benchmark {} is not registered",
                    self.options.count_function.as_deref().unwrap_or_default()
                );
                return Ok(());
            }
            Mode::Time | Mode::Count => {}
        }
        ensure!(
            self.results.cases.len() >= self.options.min_cases,
            "expected at least {} cases, measured {}",
            self.options.min_cases,
            self.results.cases.len()
        );
        // Natively timed functions are reported and stored by Criterion alone.
        let Some(directory) = self.directory.clone() else {
            return Ok(());
        };
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
        if let Some(environment) = &self.environment {
            environment.check_stable()?;
        }
        self.results.status = "complete".into();
        self.results.save(&directory)?;
        eprintln!("Benchmark artifacts: {}", directory.display());
        Ok(())
    }
}

fn counted(id: &str, counters: Metrics) -> Result<Metrics> {
    ensure!(
        counters.get("Ir").is_some_and(|v| *v > 0.0),
        "Cachegrind did not observe instructions for {id}"
    );
    Ok(counters)
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
    json!({
        "revision": git(&["rev-parse", "HEAD"]),
        "dirty": git(&["status", "--porcelain"]),
        "executable": std::env::current_exe().ok(),
        "executable_sha256": executable_digest(),
        "arguments": std::env::args().collect::<Vec<_>>(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn harness(arguments: &[&str]) -> Harness {
        let arguments = ["bench", "--list"].iter().chain(arguments);
        Harness::new(
            "fixture/target",
            Options::try_parse_from(arguments).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn the_filter_is_a_regular_expression_unless_exact() {
        let all = harness(&[]);
        assert!(all.matches("fcc/coremark/source/O2/run"));

        let regex = harness(&["coremark/.*/compile"]);
        assert!(regex.matches("fcc/coremark/source/O2/compile/core_main"));
        assert!(!regex.matches("fcc/coremark/source/O2/run"));
        assert!(!regex.matches("fcc/dhrystone/source/O2/compile/dhry_1"));

        let exact = harness(&["--exact", "pbqp/dense_search/16"]);
        assert!(exact.matches("pbqp/dense_search/16"));
        assert!(!exact.matches("pbqp/dense_search/160"));
        assert!(harness(&["pbqp/dense_search/16"]).matches("pbqp/dense_search/160"));
    }
}
