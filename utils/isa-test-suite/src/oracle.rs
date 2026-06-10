//! The oracle abstraction: something that runs a composed program and reports
//! the architectural state it ended in. Each target supplies one simulator
//! oracle (the TMDL-generated `isasim`) and one golden oracle (a reference
//! model such as Spike); the harness diffs their results.

use crate::state::ArchState;
use anyhow::Result;
use std::path::Path;

/// A fully composed program ready to hand to an oracle, plus the parameters both
/// oracles must agree on (memory layout, entry/stop labels, windows to capture).
pub struct Program {
    /// Canonical assembly source (`_start` block then the `done` stop label),
    /// laid out the way a normal assembler expects. Used by golden oracles.
    pub source: String,
    /// Same program for isasim. isasim's `ProgramImage` builder assigns label-block
    /// addresses in reverse source order, so the `done` block is written first
    /// here to land at the same address (immediately after the body).
    pub isasim_source: String,
    /// `--march` string for `isasim` (e.g. `riscv64`).
    pub isasim_march: String,
    /// Base address the image is linked/loaded at and where data memory starts.
    pub mem_base: u64,
    /// Size of the data memory window provided to both oracles.
    pub mem_size: u64,
    /// Label execution begins at (e.g. `_start`).
    pub entry: String,
    /// Label both oracles stop at, before executing it (e.g. `done`).
    pub stop: String,
    /// Memory windows `(addr, len)` to capture and compare.
    pub windows: Vec<(u64, usize)>,
}

pub trait Oracle {
    fn name(&self) -> &str;
    /// Run `prog`, using `work_dir` as scratch space for any temp files, and
    /// return the final architectural state.
    fn run(&self, prog: &Program, work_dir: &Path) -> Result<ArchState>;
}
