use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

/// Measurement engine. Profiling results never include native time or RSS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    #[default]
    Native,
    Cachegrind,
}

/// Which parts of a compiled program to measure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Phase {
    #[default]
    All,
    Compile,
    Run,
}

/// Default per-process timeout, shared by Cargo harnesses and corpus tools.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Default number of measured executions of each process case.
pub const DEFAULT_PROCESS_SAMPLES: u32 = 15;

/// Arguments understood by every Cargo benchmark target. The filter and the
/// sampling flags carry Criterion's names and meanings.
#[derive(Clone, Debug, Parser)]
pub struct Options {
    /// Run only benchmarks whose identifier matches this regular expression.
    pub filter: Option<String>,
    /// Treat the filter as a complete identifier.
    #[arg(long)]
    pub exact: bool,
    #[arg(long)]
    pub list: bool,
    /// Compiler variant for compiled-program workloads.
    #[arg(long)]
    pub compiler: Option<String>,
    /// Optimization level, e.g. O2 or -O2.
    #[arg(long, allow_hyphen_values = true)]
    pub level: Option<String>,
    #[arg(long, value_enum, default_value_t)]
    pub phase: Phase,
    /// Run function benchmarks only.
    #[arg(long)]
    pub skip_programs: bool,
    #[arg(long, value_enum, default_value_t)]
    pub engine: Engine,
    /// Samples per benchmark. Programs default to 15 and functions to Criterion's default.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub sample_size: Option<u32>,
    /// Unmeasured executions of each process case before sampling.
    #[arg(long, default_value_t = 3)]
    pub warmups: u32,
    /// Warm-up time in seconds for function benchmarks.
    #[arg(long)]
    pub warm_up_time: Option<f64>,
    /// Measurement time in seconds for function benchmarks.
    #[arg(long)]
    pub measurement_time: Option<f64>,
    /// Store native function measurements as a named Criterion baseline.
    #[arg(long)]
    pub save_baseline: Option<String>,
    /// Timeout in seconds for each subprocess, including preparation.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout: u64,
    #[arg(long)]
    pub cpu: Option<usize>,
    /// Parent directory for unique per-target result bundles.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// results.json or bundle directory from an equivalent run.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Maximum allowed time/count increase for gated cases, in percent.
    #[arg(long, default_value_t = 10.0)]
    pub threshold: f64,
    /// Maximum allowed per-case peak RSS increase, in percent.
    #[arg(long, default_value_t = 35.0)]
    pub rss_threshold: f64,
    /// Do not fetch missing source revisions.
    #[arg(long)]
    pub offline: bool,
    /// Require at least this many measured cases in this target.
    #[arg(long, default_value_t = 0)]
    pub min_cases: usize,
    #[arg(long, hide = true)]
    pub bench: bool,
    /// Count this one function benchmark. The Cachegrind parent passes it to its child.
    #[arg(long, hide = true)]
    pub count_function: Option<String>,
}
