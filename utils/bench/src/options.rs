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

/// Shared arguments understood by every Cargo benchmark target.
#[derive(Clone, Debug, Parser)]
pub struct Options {
    /// Glob over benchmark identifiers.
    #[arg(long, default_value = "*")]
    pub filter: String,
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
    #[arg(long, value_enum, default_value_t)]
    pub engine: Engine,
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u32).range(1..))]
    pub samples: u32,
    #[arg(long, default_value_t = 3)]
    pub warmups: u32,
    /// Fixed function iterations per sample; zero calibrates native functions.
    #[arg(long, default_value_t = 0)]
    pub iterations: u64,
    /// Target duration of each calibrated native function sample in milliseconds.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..))]
    pub sample_time_ms: u64,
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
    #[arg(long, hide = true)]
    pub worker: Option<String>,
}
