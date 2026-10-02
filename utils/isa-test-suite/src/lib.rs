//! Differential ISA test suite.
//!
//! Each test is an assembly snippet (the *body*). The harness wraps it in a
//! shared prologue, runs it on `isasim` (the TMDL-generated simulator under test)
//! and on a golden reference model (Spike for RISC-V), and compares the final
//! architectural state. A mismatch means the TMDL description disagrees with the
//! reference implementation.
//!
//! Entry point: [`run`], driven by `cargo xtask isa-test-suite`.

mod isasim_oracle;
mod oracle;
mod riscv;
mod state;

use anyhow::{Context, Result, bail};
use isasim_oracle::IsasimOracle;
use oracle::Oracle;
use std::path::{Path, PathBuf};

/// Run every target's suite. `isasim_bin` is the built `tir-isasim` binary.
/// Returns `true` if all tests passed (or were skipped for missing tools).
pub fn run(isasim_bin: &Path) -> Result<bool> {
    let mut all_passed = true;

    // RISC-V suite, golden model = Spike.
    let missing: Vec<&str> = riscv::REQUIRED_TOOLS
        .iter()
        .copied()
        .filter(|t| !tool_in_path(t))
        .collect();
    if !missing.is_empty() {
        println!(
            "SKIP riscv suite: missing tools on PATH: {}",
            missing.join(", ")
        );
    } else {
        if !isasim_bin.is_file() {
            bail!("isasim binary not found at {}", isasim_bin.display());
        }
        let isasim = IsasimOracle {
            bin: isasim_bin.to_path_buf(),
        };
        all_passed &= run_suite(
            "riscv",
            &suites_dir().join("riscv"),
            &isasim,
            &riscv::SpikeOracle,
            riscv::build_program,
        )?;
    }

    Ok(all_passed)
}

/// Run all `*.S` snippets in `dir`, comparing `simulator` against `golden`.
fn run_suite(
    name: &str,
    dir: &Path,
    simulator: &dyn Oracle,
    golden: &dyn Oracle,
    compose: fn(&str) -> oracle::Program,
) -> Result<bool> {
    let snippets = collect_snippets(dir)
        .with_context(|| format!("collecting snippets in {}", dir.display()))?;
    if snippets.is_empty() {
        println!("suite '{name}': no snippets found in {}", dir.display());
        return Ok(true);
    }

    println!("== suite '{name}' ({} tests) ==", snippets.len());
    let mut passed = 0usize;
    let mut failed = 0usize;

    for path in &snippets {
        let test_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match run_one(path, simulator, golden, compose) {
            Ok(diffs) if diffs.is_empty() => {
                println!("  PASS {test_name}");
                passed += 1;
            }
            Ok(diffs) => {
                println!("  FAIL {test_name}");
                for line in diffs {
                    println!("        {line}");
                }
                failed += 1;
            }
            Err(err) => {
                println!("  ERROR {test_name}: {err:#}");
                failed += 1;
            }
        }
    }

    println!("-- suite '{name}': {passed} passed, {failed} failed --");
    Ok(failed == 0)
}

/// Run a single snippet on both oracles and return the state differences.
fn run_one(
    path: &Path,
    simulator: &dyn Oracle,
    golden: &dyn Oracle,
    compose: fn(&str) -> oracle::Program,
) -> Result<Vec<String>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading snippet {}", path.display()))?;
    let program = compose(&body);

    let work = tir_adt::TempDir::with_prefix("tir-isa-test-").context("creating temp dir")?;
    let sim_state = simulator
        .run(&program, work.path())
        .with_context(|| format!("running {} oracle", simulator.name()))?;
    let golden_state = golden
        .run(&program, work.path())
        .with_context(|| format!("running {} oracle", golden.name()))?;

    Ok(sim_state.diff(&golden_state))
}

fn collect_snippets(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut snippets: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "S" || e == "s").unwrap_or(false))
        .collect();
    snippets.sort();
    Ok(snippets)
}

fn suites_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("suites")
}

/// True if `name` is an executable file on `PATH`.
fn tool_in_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
}
