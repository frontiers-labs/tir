#[path = "../../fcc/benches/coremark.rs"]
mod coremark_bench;
#[path = "../../fcc/benches/torture.rs"]
mod torture_bench;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use xshell::{cmd, Shell};

/// Nightly runners are noisy; anything under this is weather, not a regression.
const REGRESSION_THRESHOLD: f64 = 1.10;
/// The peak RSS sum is far quieter than wall time. Over three back-to-back runs
/// of the same binary it moved 0.2 %, so 2 % is ten times the observed noise.
const RSS_THRESHOLD: f64 = 1.02;
/// A single case is not quiet at all: most cases peak near the ~20 MB every fcc
/// run costs just to exist, where a couple of megabytes of allocator and thread
/// jitter is a double-digit percentage. The same three runs disagreed by up to
/// 26 % on one case, so anything under that would fail on weather. This rule is
/// here for the blowup the sum cannot see: one input growing several MB is
/// invisible against a 9.5 GB sum.
const RSS_CASE_THRESHOLD: f64 = 1.35;
const SLOWEST_SHOWN: usize = 10;
const WORST_PEAKS_SHOWN: usize = 5;
const PEAK_PREFIX: &str = "tir-mem: summary peak_vmhwm_kb=";

/// One case timed at both ends of the contract: the cheapest correct compile,
/// and the optimising one. Each fcc time is paired with the gcc time at the
/// same level, because that is the pair the KPI is about.
#[derive(Serialize, Deserialize)]
pub struct Sample {
    pub path: String,
    // A baseline written before the levels existed carries one fcc time and one
    // gcc time, which were this pipeline's `-O0`.
    #[serde(alias = "fcc_ms")]
    pub fcc_o0_ms: f64,
    #[serde(alias = "gcc_ms")]
    pub gcc_o0_ms: f64,
    #[serde(default)]
    pub fcc_o2_ms: f64,
    #[serde(default)]
    pub gcc_o2_ms: f64,
    // A baseline written before peaks were recorded carries none.
    #[serde(default)]
    pub fcc_o0_peak_kb: u64,
    #[serde(default)]
    pub fcc_o2_peak_kb: u64,
}

#[derive(Clone, Copy)]
pub(crate) enum Level {
    O0,
    O2,
}

impl Level {
    pub(crate) fn flag(self) -> &'static str {
        match self {
            Level::O0 => "-O0",
            Level::O2 => "-O2",
        }
    }

    fn fcc_ms(self, sample: &Sample) -> f64 {
        match self {
            Level::O0 => sample.fcc_o0_ms,
            Level::O2 => sample.fcc_o2_ms,
        }
    }

    fn gcc_ms(self, sample: &Sample) -> f64 {
        match self {
            Level::O0 => sample.gcc_o0_ms,
            Level::O2 => sample.gcc_o2_ms,
        }
    }

    pub(crate) fn fcc_peak_kb(self, sample: &Sample) -> u64 {
        match self {
            Level::O0 => sample.fcc_o0_peak_kb,
            Level::O2 => sample.fcc_o2_peak_kb,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Results {
    pub samples: Vec<Sample>,
    /// The many-function workload's sequential peak, recorded before
    /// parallel execution existed; `xtask gate` holds `-j8` to 1.5x of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub many_functions_o2_peak_kb: Option<u64>,
}

/// The compiler to time: `fcc` as given, or a release build of this tree.
pub(crate) fn built_fcc(sh: &Shell, root: &Path, fcc: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let fcc = match fcc {
        Some(fcc) => fcc,
        None => {
            cmd!(sh, "cargo build --release -p fcc --bin fcc").run()?;
            root.join("target/release/fcc")
        }
    };
    // Coremark units compile from their checkout, so a relative path must not
    // be resolved against that directory.
    Ok(fcc.canonicalize()?)
}

/// Every pinned case: the passing torture execute cases, then the coremark
/// units.
pub(crate) fn cases(_sh: &Shell, root: &Path) -> anyhow::Result<Vec<Case>> {
    let mut cases = Vec::new();
    for program in [
        coremark_bench::definition(root),
        torture_bench::definition(root),
    ] {
        let prepared = program.prepare(
            root,
            &tir_bench::source_cache(&root.join("target")),
            false,
            std::time::Duration::from_secs(tir_bench::DEFAULT_TIMEOUT_SECS),
        )?;
        for file in prepared.sources {
            let relative = file.strip_prefix(&prepared.directory)?;
            let label = if program.name == "torture" {
                relative.strip_prefix("execute")?
            } else {
                relative
            };
            cases.push(Case {
                label: format!("{}/{}", program.name, label.display()),
                file,
                cwd: Some(prepared.directory.clone()),
                flags: program
                    .flags
                    .iter()
                    .map(|flag| (*flag).to_owned())
                    .collect(),
            });
        }
    }
    Ok(cases)
}

/// Time every case at both levels with one worker.
pub(crate) fn measure(fcc: &Path, cases: &[Case]) -> anyhow::Result<Results> {
    let mut results = Results::default();
    let mut failed = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let times = (
            time_fcc(fcc, "-O0", case),
            time_compile(Path::new("gcc"), &["-O0"], case),
            time_fcc(fcc, "-O2", case),
            time_compile(Path::new("gcc"), &["-O2"], case),
        );
        match times {
            (
                Some((fcc_o0_ms, fcc_o0_peak_kb)),
                Some(gcc_o0_ms),
                Some((fcc_o2_ms, fcc_o2_peak_kb)),
                Some(gcc_o2_ms),
            ) => results.samples.push(Sample {
                path: case.label.clone(),
                fcc_o0_ms,
                gcc_o0_ms,
                fcc_o2_ms,
                gcc_o2_ms,
                fcc_o0_peak_kb,
                fcc_o2_peak_kb,
            }),
            _ => failed.push(case.label.clone()),
        }
        if (index + 1) % 50 == 0 || index + 1 == cases.len() {
            println!("fcc gate progress: {}/{} cases", index + 1, cases.len());
        }
    }
    if !failed.is_empty() {
        anyhow::bail!("fcc gate: failed to compile {}", failed.join(", "));
    }
    Ok(results)
}

/// Print the report, compare against `baseline` when given, and write the
/// samples to `output` when asked.
pub(crate) fn judge(
    results: &Results,
    baseline: Option<&Path>,
    output: Option<&Path>,
) -> anyhow::Result<()> {
    print!("{}", report(results));
    let mut peak_failures = Vec::new();
    if let Some(baseline) = baseline {
        let baseline: Results = serde_json::from_str(&fs::read_to_string(baseline)?)?;
        for level in [Level::O0, Level::O2] {
            let (before, after) = shared_sums(level, &baseline, results);
            println!(
                "fcc {} sum vs baseline over shared cases: {:.1} s -> {:.1} s ({:+.1} %)",
                level.flag(),
                before / 1e3,
                after / 1e3,
                (after / before - 1.0) * 100.0
            );
            if after > before * REGRESSION_THRESHOLD {
                anyhow::bail!(
                    "fcc gate: compile time at {} regressed against the baseline",
                    level.flag()
                );
            }
            peak_failures.extend(check_peaks(level, &baseline, results).err());
        }
    }
    // Written before any comparison verdict, so a failing run is diagnosable.
    if let Some(output) = output {
        fs::write(output, serde_json::to_string_pretty(results)?)?;
    }
    if let Some(failure) = peak_failures.into_iter().next() {
        return Err(failure);
    }
    Ok(())
}

pub(crate) struct Case {
    pub label: String,
    pub file: PathBuf,
    pub cwd: Option<PathBuf>,
    pub flags: Vec<String>,
}

impl Case {
    pub(crate) fn is_coremark(&self) -> bool {
        self.label.starts_with("coremark/")
    }
}

/// `compiler -c` on `case` with `extra` flags, writing the object to `output`.
pub(crate) fn compile_command(
    compiler: &Path,
    extra: &[&str],
    case: &Case,
    output: &Path,
) -> Command {
    let mut command = Command::new(compiler);
    command
        .arg("-std=gnu17")
        .args(extra)
        .args(&case.flags)
        .args(["-c", "-o"])
        .arg(output)
        .arg(&case.file)
        .stdout(Stdio::null());
    if let Some(cwd) = &case.cwd {
        command.current_dir(cwd);
    }
    command
}

fn time_compile(compiler: &Path, extra: &[&str], case: &Case) -> Option<f64> {
    let mut command = compile_command(compiler, extra, case, Path::new("/dev/null"));
    command.stderr(Stdio::null());
    let started = Instant::now();
    let success = command.status().is_ok_and(|status| status.success());
    success.then(|| started.elapsed().as_secs_f64() * 1e3)
}

/// Times fcc and reads its peak RSS from the same process. Memory reporting
/// costs one `/proc` read per pass, which is noise against the time threshold
/// and is paid identically by the baseline and the candidate. It is switched on
/// through the environment because `--mem-report` is a flag of fcc's own CLI,
/// and the bench drives the gcc-compatible one.
fn time_fcc(fcc: &Path, level: &str, case: &Case) -> Option<(f64, u64)> {
    let (ms, peak_kb, _) = time_fcc_to(fcc, &[level], case, Path::new("/dev/null"))?;
    Some((ms, peak_kb))
}

/// [`time_fcc`] with the object written to `output`, handing back fcc's
/// stderr too, which carries the memory report.
pub(crate) fn time_fcc_to(
    fcc: &Path,
    flags: &[&str],
    case: &Case,
    output: &Path,
) -> Option<(f64, u64, String)> {
    let mut command = compile_command(fcc, flags, case, output);
    command.env("TIR_MEM_STATS", "1").stderr(Stdio::piped());
    let started = Instant::now();
    let run = command.output().ok()?;
    let ms = started.elapsed().as_secs_f64() * 1e3;
    if !run.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    let peak_kb = parse_peak_kb(&stderr)?;
    Some((ms, peak_kb, stderr))
}

/// The per-case peaks at `level`, by label.
pub(crate) fn peaks_by_label(level: Level, results: &Results) -> HashMap<&str, u64> {
    results
        .samples
        .iter()
        .map(|sample| (sample.path.as_str(), level.fcc_peak_kb(sample)))
        .collect()
}

/// A run prints one summary per pass manager it drives, and `VmHWM` only ever
/// grows, so the largest is the process peak.
fn parse_peak_kb(stderr: &str) -> Option<u64> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix(PEAK_PREFIX))
        .filter_map(|value| value.trim().parse().ok())
        .max()
}

fn fcc_sum(level: Level, results: &Results) -> f64 {
    results
        .samples
        .iter()
        .map(|sample| level.fcc_ms(sample))
        .sum()
}

fn median_ratio(level: Level, results: &Results) -> f64 {
    let mut ratios = results
        .samples
        .iter()
        .map(|sample| level.fcc_ms(sample) / level.gcc_ms(sample))
        .collect::<Vec<_>>();
    ratios.sort_by(|a, b| a.total_cmp(b));
    match ratios.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => ratios[n / 2],
        n => (ratios[n / 2 - 1] + ratios[n / 2]) / 2.0,
    }
}

/// What both runs measured for the cases they share, as
/// `(path, baseline, current)`. A baseline that measured nothing for a case
/// says nothing about it, so that case is dropped.
fn shared_cases<'a, T: Copy + Default + PartialOrd>(
    baseline: &Results,
    current: &'a Results,
    measure: impl Fn(&Sample) -> T,
) -> Vec<(&'a str, T, T)> {
    let before = baseline
        .samples
        .iter()
        .map(|sample| (sample.path.as_str(), measure(sample)))
        .collect::<HashMap<_, _>>();
    current
        .samples
        .iter()
        .filter_map(|sample| {
            before
                .get(sample.path.as_str())
                .filter(|measured| **measured > T::default())
                .map(|measured| (sample.path.as_str(), *measured, measure(sample)))
        })
        .collect()
}

fn shared_sums(level: Level, baseline: &Results, current: &Results) -> (f64, f64) {
    shared_cases(baseline, current, |sample| level.fcc_ms(sample))
        .into_iter()
        .fold((0.0, 0.0), |(b, a), (_, before, after)| {
            (b + before, a + after)
        })
}

fn shared_peak_sums(level: Level, baseline: &Results, current: &Results) -> (f64, f64) {
    shared_peaks(level, baseline, current)
        .into_iter()
        .fold((0.0, 0.0), |(b, a), (_, before, after)| {
            (b + before as f64, a + after as f64)
        })
}

fn shared_peaks<'a>(
    level: Level,
    baseline: &Results,
    current: &'a Results,
) -> Vec<(&'a str, u64, u64)> {
    shared_cases(baseline, current, |sample| level.fcc_peak_kb(sample))
}

fn check_peaks(level: Level, baseline: &Results, current: &Results) -> anyhow::Result<()> {
    let (before, after) = shared_peak_sums(level, baseline, current);
    if before == 0.0 {
        return Ok(());
    }
    println!(
        "fcc {} peak sum vs baseline over shared cases: {:.0} MB -> {:.0} MB ({:+.1} %)",
        level.flag(),
        before / 1e3,
        after / 1e3,
        (after / before - 1.0) * 100.0
    );
    let mut worst = shared_peaks(level, baseline, current);
    worst.sort_by(|a, b| (b.2 as f64 / b.1 as f64).total_cmp(&(a.2 as f64 / a.1 as f64)));
    for (path, before, after) in worst.iter().take(WORST_PEAKS_SHOWN) {
        println!(
            "  {:>8} kB -> {:>8} kB  ({:+.1} %)  {path}",
            before,
            after,
            (*after as f64 / *before as f64 - 1.0) * 100.0
        );
    }
    if after > before * RSS_THRESHOLD {
        anyhow::bail!(
            "fcc gate: peak RSS at {} regressed against the baseline",
            level.flag()
        );
    }
    if let Some((path, before, after)) = worst
        .first()
        .filter(|(_, before, after)| *after as f64 > *before as f64 * RSS_CASE_THRESHOLD)
    {
        anyhow::bail!(
            "fcc gate: peak RSS at {} regressed on {path}: {before} kB -> {after} kB",
            level.flag()
        );
    }
    Ok(())
}

fn report(results: &Results) -> String {
    let mut out = format!("fcc gate: {} cases\n", results.samples.len());
    for level in [Level::O0, Level::O2] {
        let gcc_sum: f64 = results
            .samples
            .iter()
            .map(|sample| level.gcc_ms(sample))
            .sum();
        let peak_sum: u64 = results
            .samples
            .iter()
            .map(|sample| level.fcc_peak_kb(sample))
            .sum();
        out.push_str(&format!(
            "  fcc {flag} {:.1} s, gcc {flag} {:.1} s, median ratio {:.1}x, fcc peak sum {:.0} MB\n",
            fcc_sum(level, results) / 1e3,
            gcc_sum / 1e3,
            median_ratio(level, results),
            peak_sum as f64 / 1e3,
            flag = level.flag(),
        ));
    }
    let mut slowest = results.samples.iter().collect::<Vec<_>>();
    slowest.sort_by(|a, b| b.fcc_o2_ms.total_cmp(&a.fcc_o2_ms));
    for sample in slowest.iter().take(SLOWEST_SHOWN) {
        out.push_str(&format!(
            "  {:>9.1} ms  {:>7.1} ms  {}\n",
            sample.fcc_o2_ms, sample.gcc_o2_ms, sample.path
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_kb_reads_the_summary_line() {
        let stderr = "tir-mem: pbqp label=f nodes=3 edges=2 matrix_bytes=64\n\
                      tir-mem: summary peak_vmhwm_kb=12345\n\
                      tir-mem: top-pass name=instcombine hwm_delta_kb=7\n";
        assert_eq!(parse_peak_kb(stderr), Some(12345));
    }

    #[test]
    fn peak_kb_takes_the_process_peak_across_summaries() {
        let stderr = "tir-mem: summary peak_vmhwm_kb=16684\n\
                      tir-mem: summary peak_vmhwm_kb=20160\n\
                      tir-mem: summary peak_vmhwm_kb=17248\n";
        assert_eq!(parse_peak_kb(stderr), Some(20160));
    }

    #[test]
    fn peak_kb_is_absent_without_the_summary_line() {
        assert_eq!(parse_peak_kb("cc1: warning: nothing to see here\n"), None);
    }

    #[test]
    fn shared_peak_sums_ignore_cases_missing_on_either_side() {
        let baseline = results(&[("a", 100), ("b", 200), ("only-baseline", 900)]);
        let current = results(&[("a", 110), ("b", 190), ("only-current", 900)]);
        assert_eq!(
            shared_peak_sums(Level::O2, &baseline, &current),
            (300.0, 300.0)
        );
    }

    #[test]
    fn check_peaks_tolerates_single_case_jitter() {
        let cold = (0..1000).map(|index| (format!("cold{index}"), 10_000));
        let baseline = results_owned(cold.clone().chain([("jittery".to_string(), 10_000)]));
        let current = results_owned(cold.chain([("jittery".to_string(), 12_500)]));
        assert!(check_peaks(Level::O2, &baseline, &current).is_ok());
    }

    #[test]
    fn check_peaks_fails_on_the_sum() {
        let baseline = results(&[("a", 100_000), ("b", 100_000)]);
        let current = results(&[("a", 103_000), ("b", 103_000)]);
        assert!(check_peaks(Level::O2, &baseline, &current).is_err());
    }

    #[test]
    fn check_peaks_fails_on_one_case_the_sum_hides() {
        let cold = (0..1000).map(|index| (format!("cold{index}"), 10_000));
        let baseline = results_owned(cold.clone().chain([("hot".to_string(), 10_000)]));
        let current = results_owned(cold.chain([("hot".to_string(), 20_000)]));
        let error = check_peaks(Level::O2, &baseline, &current)
            .expect_err("a doubled case must fail even when the sum moves 0.1 %");
        assert!(error.to_string().contains("hot"), "{error}");
    }

    fn results(cases: &[(&str, u64)]) -> Results {
        results_owned(
            cases
                .iter()
                .map(|(path, peak)| ((*path).to_string(), *peak)),
        )
    }

    fn results_owned(cases: impl IntoIterator<Item = (String, u64)>) -> Results {
        Results {
            many_functions_o2_peak_kb: None,
            samples: cases
                .into_iter()
                .map(|(path, peak)| Sample {
                    path,
                    fcc_o0_ms: 1.0,
                    gcc_o0_ms: 1.0,
                    fcc_o2_ms: 1.0,
                    gcc_o2_ms: 1.0,
                    fcc_o0_peak_kb: peak,
                    fcc_o2_peak_kb: peak,
                })
                .collect(),
        }
    }
}
