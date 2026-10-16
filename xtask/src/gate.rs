//! The pinned performance gate, and the parallel contract on top of it.
//!
//! Sequential (`-j1`) timings and peaks are judged against the pinned
//! baseline using the original GCC comparison contract. Then every pinned case is
//! compiled again with `-j8`: the object must be the bytes `-j1` produced, and
//! the peak may not pass [`PARALLEL_RSS_THRESHOLD`] times the pinned
//! sequential peak, per case and in sum. CoreMark's wall time is recorded at
//! both counts. Last, a generated many-function unit is compiled at both
//! counts: same object, and a `-j8` peak within the threshold of the
//! sequential reference the baseline recorded before parallel execution
//! existed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use xshell::Shell;

use crate::gate_bench::{
    built_fcc, cases, judge, measure, peaks_by_label, time_fcc_to, Case, Level, Results,
};

/// How far a parallel peak may pass the pinned sequential one.
const PARALLEL_RSS_THRESHOLD: f64 = 1.5;
const EPOCH_PREFIX: &str = "tir-mem: epoch ";

#[derive(clap::Args)]
pub struct Options {
    /// An already built compiler to gate, instead of building a release one.
    #[arg(long)]
    pub fcc: Option<PathBuf>,
    /// Where to write this run's samples.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The pinned samples to compare against.
    #[arg(long)]
    pub baseline: PathBuf,
    /// The parallel worker count to hold to the contract.
    #[arg(long, default_value_t = 8)]
    pub jobs: usize,
    /// Functions in the generated many-function unit.
    #[arg(long, default_value_t = 400)]
    pub functions: usize,
}

pub fn run(sh: &Shell, root: &Path, options: Options) -> anyhow::Result<()> {
    let fcc = built_fcc(sh, root, options.fcc)?;
    let cases = cases(sh, root)?;
    let scratch = root.join("target/gate");
    fs::create_dir_all(&scratch)?;

    let baseline: Results = serde_json::from_str(&fs::read_to_string(&options.baseline)?)?;
    let many = many_functions(
        &fcc,
        options.jobs,
        options.functions,
        baseline.many_functions_o2_peak_kb,
        &scratch,
    );
    let mut results = measure(&fcc, &cases)?;
    let sequential = judge(&results, Some(&options.baseline), None);
    let parallel = parallel_pass(&fcc, &cases, options.jobs, &baseline, &scratch);
    if let Ok(peak) = &many {
        results.many_functions_o2_peak_kb = Some(*peak);
    }
    if let Some(output) = &options.output {
        fs::write(output, serde_json::to_string_pretty(&results)?)?;
    }
    sequential?;
    parallel?;
    many.map(drop)
}

fn parallel_pass(
    fcc: &Path,
    cases: &[Case],
    jobs: usize,
    baseline: &Results,
    scratch: &Path,
) -> anyhow::Result<()> {
    let jobs_flag = format!("-j{jobs}");
    let pinned = peaks_by_label(Level::O2, baseline);
    let mut peak_sum = (0u64, 0u64);
    let mut worst: Option<(f64, String, u64, u64)> = None;
    let mut mismatched = Vec::new();
    let mut failed = Vec::new();
    let mut coremark = Vec::new();
    for case in cases {
        let one = scratch.join("j1.o");
        let many = scratch.join("jn.o");
        let sequential = time_fcc_to(fcc, &["-O2"], case, &one);
        let parallel = time_fcc_to(fcc, &["-O2", &jobs_flag], case, &many);
        let (Some((ms_one, _, _)), Some((ms_many, peak_many, _))) = (sequential, parallel) else {
            failed.push(case.label.clone());
            continue;
        };
        if fs::read(&one)? != fs::read(&many)? {
            mismatched.push(case.label.clone());
        }
        if case.is_coremark() {
            coremark.push((case.label.clone(), ms_one, ms_many));
        }
        if let Some(&before) = pinned.get(case.label.as_str()).filter(|peak| **peak > 0) {
            peak_sum.0 += before;
            peak_sum.1 += peak_many;
            let ratio = peak_many as f64 / before as f64;
            if worst.as_ref().is_none_or(|(held, ..)| ratio > *held) {
                worst = Some((ratio, case.label.clone(), before, peak_many));
            }
        }
    }
    println!("fcc gate: coremark wall time at -j1 and -j{jobs}");
    let (mut sum_one, mut sum_many) = (0.0, 0.0);
    for (label, ms_one, ms_many) in &coremark {
        println!("  {ms_one:>8.1} ms  {ms_many:>8.1} ms  {label}");
        sum_one += ms_one;
        sum_many += ms_many;
    }
    println!(
        "  coremark sum {sum_one:.1} ms -> {sum_many:.1} ms ({:+.1} %)",
        (sum_many / sum_one - 1.0) * 100.0
    );
    println!(
        "fcc gate: -O2 peak sum at -j{jobs} vs pinned -j1: {:.0} MB -> {:.0} MB ({:+.1} %)",
        peak_sum.0 as f64 / 1e3,
        peak_sum.1 as f64 / 1e3,
        (peak_sum.1 as f64 / peak_sum.0 as f64 - 1.0) * 100.0
    );
    if let Some((ratio, label, before, after)) = &worst {
        println!("  worst case {before} kB -> {after} kB ({ratio:.2}x) {label}");
    }
    if !failed.is_empty() {
        anyhow::bail!("fcc gate: failed to compile {}", failed.join(", "));
    }
    if !mismatched.is_empty() {
        anyhow::bail!(
            "fcc gate: -j{jobs} objects differ from -j1 on {}",
            mismatched.join(", ")
        );
    }
    if peak_sum.1 as f64 > peak_sum.0 as f64 * PARALLEL_RSS_THRESHOLD {
        anyhow::bail!(
            "fcc gate: -j{jobs} peak RSS sum passed {PARALLEL_RSS_THRESHOLD}x the pinned sequential sum"
        );
    }
    if let Some((ratio, label, before, after)) =
        worst.filter(|(ratio, ..)| *ratio > PARALLEL_RSS_THRESHOLD)
    {
        anyhow::bail!(
            "fcc gate: -j{jobs} peak RSS on {label} is {ratio:.2}x the pinned {before} kB ({after} kB)"
        );
    }
    Ok(())
}

fn many_functions(
    fcc: &Path,
    jobs: usize,
    functions: usize,
    reference_kb: Option<u64>,
    scratch: &Path,
) -> anyhow::Result<u64> {
    let source = scratch.join("many_functions.c");
    fs::write(&source, generate(functions, 1))?;
    let case = Case {
        label: "many-functions".to_string(),
        file: source,
        cwd: None,
        flags: Vec::new(),
    };
    let one = scratch.join("many_j1.o");
    let many = scratch.join("many_jn.o");
    let Some((ms_one, peak_one, _)) = time_fcc_to(fcc, &["-O2"], &case, &one) else {
        anyhow::bail!("fcc gate: the many-function unit failed to compile at -j1");
    };
    let jobs_flag = format!("-j{jobs}");
    let Some((ms_many, peak_many, stderr)) = time_fcc_to(fcc, &["-O2", &jobs_flag], &case, &many)
    else {
        anyhow::bail!("fcc gate: the many-function unit failed to compile at -j{jobs}");
    };
    println!(
        "fcc gate: many-function unit ({functions} functions) -j1 {ms_one:.1} ms {peak_one} kB, \
         -j{jobs} {ms_many:.1} ms {peak_many} kB"
    );
    for line in epoch_summary(&stderr) {
        println!("  {line}");
    }
    if fs::read(&one)? != fs::read(&many)? {
        anyhow::bail!("fcc gate: many-function objects differ between -j1 and -j{jobs}");
    }
    let Some(reference) = reference_kb else {
        println!("  no sequential reference pinned; this run's -j1 peak is written as one");
        return Ok(peak_one);
    };
    println!(
        "  -j{jobs} peak vs pinned sequential reference: {reference} kB -> {peak_many} kB ({:.2}x)",
        peak_many as f64 / reference as f64
    );
    if peak_many as f64 > reference as f64 * PARALLEL_RSS_THRESHOLD {
        anyhow::bail!(
            "fcc gate: many-function peak RSS at -j{jobs} passed {PARALLEL_RSS_THRESHOLD}x the pinned reference"
        );
    }
    Ok(reference)
}

fn epoch_summary(stderr: &str) -> Vec<String> {
    let mut largest: HashMap<&str, u64> = HashMap::new();
    for line in stderr
        .lines()
        .filter_map(|line| line.strip_prefix(EPOCH_PREFIX))
    {
        for field in line.split_whitespace() {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            if let Ok(value) = value.parse::<u64>() {
                let held = largest.entry(key).or_default();
                *held = (*held).max(value);
            }
        }
    }
    let mut keys: Vec<_> = largest.keys().copied().collect();
    keys.sort_unstable();
    keys.into_iter()
        .map(|key| format!("epoch peak {key}={}", largest[key]))
        .collect()
}

pub(crate) fn generate(functions: usize, seed: u64) -> String {
    let mut state = seed;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };
    let mut source = String::from("int leaf(int x) { return x * 3 + 1; }\n");
    for index in 0..functions {
        let previous = if index == 0 {
            "leaf".to_string()
        } else {
            format!("f{}", index - 1)
        };
        let (a, b, c) = (next() % 97 + 1, next() % 89 + 1, next() % 13 + 3);
        source.push_str(&format!(
            "int f{index}(int x) {{\n  int a[8];\n  for (int k = 0; k < 8; k++) a[k] = x * {a} + k * {b};\n  int s = 0;\n  for (int k = 0; k < 8; k++) s += a[k] ^ {c};\n  return s + {previous}(x - 1);\n}}\n"
        ));
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_is_fixed_by_its_count_and_seed() {
        assert_eq!(generate(3, 1), generate(3, 1));
        assert_ne!(generate(3, 1), generate(3, 2));
        assert_eq!(generate(3, 1).matches("int f").count(), 3);
    }

    #[test]
    fn the_epoch_summary_keeps_the_largest_of_each_measure() {
        let stderr = "tir-mem: epoch nest=func.func tasks=2 batches_retained=2 base_bytes=10 \
                      active_overlay_peak_bytes=5 retained_batch_bytes=7\n\
                      tir-mem: epoch nest=func.func tasks=3 batches_retained=3 base_bytes=12 \
                      active_overlay_peak_bytes=4 retained_batch_bytes=9\n";
        assert_eq!(
            epoch_summary(stderr),
            [
                "epoch peak active_overlay_peak_bytes=5",
                "epoch peak base_bytes=12",
                "epoch peak batches_retained=3",
                "epoch peak retained_batch_bytes=9",
                "epoch peak tasks=3",
            ]
        );
    }
}
