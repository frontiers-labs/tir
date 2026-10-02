//! Cargo benchmark harness with Criterion's interface.
//!
//! Function benchmarks are timed by Criterion itself. External programs are
//! compiled, verified and measured as processes. Under `--engine cachegrind`
//! both kinds report instruction, cache and branch counts in one result bundle.
mod cachegrind;
pub mod environment;
mod harness;
mod options;
pub mod process;
pub mod program;
mod results;
pub mod sources;

pub use anyhow::Result;
pub use criterion::{BatchSize, Throughput};
pub use options::{DEFAULT_TIMEOUT_SECS, Engine, Options, Phase};
pub use process::Command;
pub use program::Program;
pub use results::Variant;

use clap::Parser;
use criterion::measurement::WallTime;
use harness::{Harness, Mode};
use serde_json::{Value, json};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Define a function that runs the listed benchmark functions in order.
#[macro_export]
macro_rules! criterion_group {
    ($name:ident, $($target:path),+ $(,)?) => {
        pub fn $name(criterion: &mut $crate::Criterion) {
            $($target(criterion);)+
        }
    };
}

/// Define `main` for a Cargo benchmark target with `harness = false`.
#[macro_export]
macro_rules! criterion_main {
    ($($group:path),+ $(,)?) => {
        #[allow(dead_code)] // Tools import benchmark definitions without running them.
        fn main() -> $crate::Result<()> {
            $crate::run(
                $crate::Target {
                    package: env!("CARGO_PKG_NAME"),
                    name: env!("CARGO_CRATE_NAME"),
                    fcc: option_env!("CARGO_BIN_EXE_fcc"),
                    tir: option_env!("CARGO_BIN_EXE_tir"),
                },
                &[$($group),+],
            )
        }
    };
}

/// The Cargo benchmark target being run and the compilers Cargo built for it.
pub struct Target {
    pub package: &'static str,
    pub name: &'static str,
    /// Path of the package's `fcc` binary, which compiles programs from source.
    pub fcc: Option<&'static str>,
    /// Path of the package's `tir` binary, which compiles programs from LLVM IR.
    pub tir: Option<&'static str>,
}

/// Entry point behind [`criterion_main!`].
pub fn run(target: Target, groups: &[fn(&mut Criterion)]) -> Result<()> {
    let namespace = format!("{}/{}", target.package, target.name);
    let mut criterion = Criterion::new(&namespace, Options::parse())?;
    criterion.harness.compilers = match (target.fcc, target.tir) {
        (Some(fcc), _) => Some(program::CompilerSet::source(fcc)),
        (None, Some(tir)) => Some(program::CompilerSet::llvm(tir)),
        (None, None) => None,
    };
    for group in groups {
        group(&mut criterion);
    }
    criterion.finish()
}

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
    /// Set when other cases measure the same benchmark in another variant.
    pub variant: Option<Variant>,
}

/// A benchmark name with a parameter, formatted as `name/parameter`.
pub struct BenchmarkId(String);

impl BenchmarkId {
    pub fn new(name: impl Into<String>, parameter: impl std::fmt::Display) -> Self {
        Self(format!("{}/{parameter}", name.into()))
    }
    pub fn from_parameter(parameter: impl std::fmt::Display) -> Self {
        Self(parameter.to_string())
    }
}

impl<S: Into<String>> From<S> for BenchmarkId {
    fn from(name: S) -> Self {
        Self(name.into())
    }
}

/// One Cargo benchmark target. All measured work runs serially under a host lock.
pub struct Criterion {
    harness: Harness,
    /// Times functions natively. Built on first use, since listing and counting never need it.
    native: Option<criterion::Criterion>,
}

impl Criterion {
    /// Create a harness with explicit options. Listing does not acquire a host lock.
    pub fn new(namespace: &str, options: Options) -> Result<Self> {
        Ok(Self {
            harness: Harness::new(namespace, options)?,
            native: None,
        })
    }

    pub fn benchmark_group(&mut self, name: impl Into<String>) -> BenchmarkGroup<'_> {
        let name = name.into();
        let native = (self.harness.mode == Mode::Time).then(|| {
            let options = &self.harness.options;
            self.native
                .get_or_insert_with(|| native_criterion(options))
                .benchmark_group(name.clone())
        });
        BenchmarkGroup {
            harness: &mut self.harness,
            name,
            native,
        }
    }

    /// Benchmark a function outside any group. `id` is its whole identifier.
    pub fn bench_function(
        &mut self,
        id: &str,
        mut routine: impl FnMut(&mut Bencher<'_, '_>),
    ) -> &mut Self {
        match self.harness.function(id) {
            Some(Measure::Count) => routine(&mut Bencher(Measure::Count)),
            Some(Measure::Time(())) => {
                let options = &self.harness.options;
                self.native
                    .get_or_insert_with(|| native_criterion(options))
                    .bench_function(id, |bencher| {
                        routine(&mut Bencher(Measure::Time(bencher)));
                    });
            }
            None => {}
        }
        self
    }

    /// Compile, verify and measure an external program with the package's compiler
    /// and its reference compilers. A failure stops the target and is reported by `main`.
    pub fn bench_program(&mut self, program: &Program) -> &mut Self {
        self.harness
            .attempt(|harness| program::measure(harness, program));
        self
    }

    /// Measure prepared processes in rotated order, validating each execution.
    pub fn bench_processes(&mut self, cases: Vec<ProcessCase>) -> &mut Self {
        self.harness.attempt(|harness| harness.process_group(cases));
        self
    }

    /// Directory of this run's result bundle, created on first use.
    pub fn artifacts(&mut self) -> Result<PathBuf> {
        self.harness.bundle()
    }

    /// Report the first failure, count pending functions, and complete the bundle.
    pub fn finish(mut self) -> Result<()> {
        if let Some(failure) = self.harness.failure.take() {
            return Err(failure);
        }
        self.harness.count_functions()?;
        if let Some(native) = &self.native {
            native.final_summary();
        }
        self.harness.finish()
    }
}

fn native_criterion(options: &Options) -> criterion::Criterion {
    let mut criterion = criterion::Criterion::default();
    if let Some(samples) = options.sample_size {
        // Criterion rejects fewer than 10 samples. Smaller values still apply to processes.
        if samples < 10 {
            eprintln!("--sample-size {samples} is below Criterion's minimum; functions use 10");
        }
        criterion = criterion.sample_size(samples.max(10) as usize);
    }
    if let Some(seconds) = options.warm_up_time {
        criterion = criterion.warm_up_time(Duration::from_secs_f64(seconds));
    }
    if let Some(seconds) = options.measurement_time {
        criterion = criterion.measurement_time(Duration::from_secs_f64(seconds));
    }
    if let Some(name) = &options.save_baseline {
        criterion = criterion.save_baseline(name.clone());
    }
    criterion
}

/// Benchmarks that share a name prefix and sampling settings.
pub struct BenchmarkGroup<'a> {
    harness: &'a mut Harness,
    name: String,
    native: Option<criterion::BenchmarkGroup<'a, WallTime>>,
}

impl BenchmarkGroup<'_> {
    // Sampling settings shape native timing only. A command-line value wins over
    // the benchmark's own, so a quick local run needs no source edit.
    pub fn sample_size(&mut self, samples: usize) -> &mut Self {
        if let Some(native) = &mut self.native
            && self.harness.options.sample_size.is_none()
        {
            native.sample_size(samples);
        }
        self
    }

    pub fn warm_up_time(&mut self, duration: Duration) -> &mut Self {
        if let Some(native) = &mut self.native
            && self.harness.options.warm_up_time.is_none()
        {
            native.warm_up_time(duration);
        }
        self
    }

    pub fn measurement_time(&mut self, duration: Duration) -> &mut Self {
        if let Some(native) = &mut self.native
            && self.harness.options.measurement_time.is_none()
        {
            native.measurement_time(duration);
        }
        self
    }

    pub fn throughput(&mut self, throughput: Throughput) -> &mut Self {
        if let Some(native) = &mut self.native {
            native.throughput(throughput);
        }
        self
    }

    pub fn bench_function(
        &mut self,
        id: impl Into<BenchmarkId>,
        mut routine: impl FnMut(&mut Bencher<'_, '_>),
    ) -> &mut Self {
        let BenchmarkId(name) = id.into();
        match self.harness.function(&format!("{}/{name}", self.name)) {
            Some(Measure::Count) => routine(&mut Bencher(Measure::Count)),
            Some(Measure::Time(())) => {
                let native = self.native.as_mut().expect("timed groups wrap Criterion");
                native.bench_function(name, |bencher| {
                    routine(&mut Bencher(Measure::Time(bencher)));
                });
            }
            None => {}
        }
        self
    }

    pub fn bench_with_input<I: ?Sized>(
        &mut self,
        id: impl Into<BenchmarkId>,
        input: &I,
        mut routine: impl FnMut(&mut Bencher<'_, '_>, &I),
    ) -> &mut Self {
        self.bench_function(id, |bencher| routine(bencher, input))
    }

    pub fn finish(self) {
        if let Some(native) = self.native {
            native.finish();
        }
    }
}

/// How a function benchmark's routine is measured.
pub(crate) enum Measure<T> {
    /// Criterion samples the routine and reports wall time.
    Time(T),
    /// The routine runs once with Cachegrind counting its region.
    Count,
}

/// Runs the measured operation of one function benchmark.
pub struct Bencher<'a, 'b>(Measure<&'a mut criterion::Bencher<'b, WallTime>>);

impl Bencher<'_, '_> {
    /// Measure the routine, including destruction of its return value.
    pub fn iter<O>(&mut self, mut routine: impl FnMut() -> O) {
        match &mut self.0 {
            Measure::Time(bencher) => bencher.iter(routine),
            Measure::Count => {
                cachegrind::start();
                black_box(routine());
                cachegrind::stop();
            }
        }
    }

    /// Exclude input preparation and output destruction from measurement.
    pub fn iter_batched<I, O>(
        &mut self,
        mut setup: impl FnMut() -> I,
        mut routine: impl FnMut(I) -> O,
        size: BatchSize,
    ) {
        match &mut self.0 {
            Measure::Time(bencher) => bencher.iter_batched(setup, routine, size),
            Measure::Count => {
                let input = black_box(setup());
                cachegrind::start();
                let output = black_box(routine(input));
                cachegrind::stop();
                drop(output);
            }
        }
    }
}

/// Build settings of the harness and its Cargo-built compiler dependencies.
pub fn build_configuration() -> Value {
    json!({
        "profile": env!("TIR_BENCH_PROFILE"),
        "opt_level": env!("TIR_BENCH_OPT_LEVEL"),
        "debug": env!("TIR_BENCH_DEBUG"),
        "target": env!("TIR_BENCH_TARGET"),
        "rustflags": env!("TIR_BENCH_CARGO_ENCODED_RUSTFLAGS"),
        "rustc": env!("TIR_BENCH_RUSTC"),
    })
}

/// Resolve the shared pinned-source cache, honouring an explicit environment override.
pub fn source_cache(target: &Path) -> PathBuf {
    std::env::var_os("TIR_BENCH_SOURCE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| target.join("bench-sources"))
}
