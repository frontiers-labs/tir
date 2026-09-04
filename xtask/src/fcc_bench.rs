use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use xshell::{cmd, Shell};

use crate::fcc_torture::execute_corpus;

const COREMARK_REPOSITORY: &str = "https://github.com/eembc/coremark.git";
const COREMARK_REVISION: &str = "1f483d5b8316753a742cbf5590caf5bd0a4e4777";
const COREMARK_PATH: &str = "target/test-suites/coremark";
const COREMARK_UNITS: &[&str] = &[
    "core_list_join.c",
    "core_main.c",
    "core_matrix.c",
    "core_state.c",
    "core_util.c",
    "posix/core_portme.c",
];
const COREMARK_FLAGS: &[&str] = &[
    "-I.",
    "-Iposix",
    "-DFLAGS_STR=\"\"",
    "-DPERFORMANCE_RUN=1",
    "-DITERATIONS=1000",
];
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

#[derive(clap::Args)]
pub struct Options {
    /// An already built compiler to time, instead of building a debug one.
    #[arg(long)]
    pub fcc: Option<PathBuf>,
    /// Where to write this run's samples.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// A previous run's samples to compare against.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
}

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
enum Level {
    O0,
    O2,
}

impl Level {
    fn flag(self) -> &'static str {
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

    fn fcc_peak_kb(self, sample: &Sample) -> u64 {
        match self {
            Level::O0 => sample.fcc_o0_peak_kb,
            Level::O2 => sample.fcc_o2_peak_kb,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Results {
    pub samples: Vec<Sample>,
}

/// Times fcc and gcc at both `-O0` and `-O2` on every passing torture execute
/// case and the
/// coremark translation units, one at a time so the numbers are wall time on an
/// idle machine. Fails when a case does not compile or when the fcc sum over
/// the cases both runs share grew more than [`REGRESSION_THRESHOLD`] over the
/// baseline. There is no per-case timeout: a hung compiler is the job's
/// timeout to catch, and a poll loop would quantise the samples.
pub fn run(sh: &Shell, root: &Path, options: Options) -> anyhow::Result<()> {
    let fcc = match options.fcc {
        Some(fcc) => fcc,
        None => {
            cmd!(sh, "cargo build --release -p fcc --bin fcc").run()?;
            root.join("target/release/fcc")
        }
    };
    // Coremark units compile from their checkout, so a relative path must not
    // be resolved against that directory.
    let fcc = fcc.canonicalize()?;
    let mut cases = execute_corpus(sh, root)?
        .into_iter()
        .map(|file| Case {
            label: format!("torture/{}", file.file_name().unwrap().to_string_lossy()),
            file,
            cwd: None,
            flags: Vec::new(),
        })
        .collect::<Vec<_>>();
    let coremark = fetch_coremark(sh, root)?;
    cases.extend(COREMARK_UNITS.iter().map(|unit| Case {
        label: format!("coremark/{unit}"),
        file: coremark.join(unit),
        cwd: Some(coremark.clone()),
        flags: COREMARK_FLAGS.iter().map(|flag| flag.to_string()).collect(),
    }));

    let mut results = Results::default();
    let mut failed = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let times = (
            time_fcc(&fcc, "-O0", case),
            time_compile(Path::new("gcc"), &["-O0"], case),
            time_fcc(&fcc, "-O2", case),
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
            println!("fcc bench progress: {}/{} cases", index + 1, cases.len());
        }
    }

    print!("{}", report(&results));
    if !failed.is_empty() {
        anyhow::bail!("fcc bench: failed to compile {}", failed.join(", "));
    }
    let mut peak_failures = Vec::new();
    if let Some(baseline) = &options.baseline {
        let baseline: Results = serde_json::from_str(&fs::read_to_string(baseline)?)?;
        for level in [Level::O0, Level::O2] {
            let (before, after) = shared_sums(level, &baseline, &results);
            println!(
                "fcc {} sum vs baseline over shared cases: {:.1} s -> {:.1} s ({:+.1} %)",
                level.flag(),
                before / 1e3,
                after / 1e3,
                (after / before - 1.0) * 100.0
            );
            if after > before * REGRESSION_THRESHOLD {
                anyhow::bail!(
                    "fcc bench: compile time at {} regressed against the baseline",
                    level.flag()
                );
            }
            peak_failures.extend(check_peaks(level, &baseline, &results).err());
        }
    }
    // Written before any comparison verdict, so a failing run is diagnosable.
    if let Some(output) = &options.output {
        fs::write(output, serde_json::to_string_pretty(&results)?)?;
    }
    if let Some(failure) = peak_failures.into_iter().next() {
        return Err(failure);
    }
    Ok(())
}

struct Case {
    label: String,
    file: PathBuf,
    cwd: Option<PathBuf>,
    flags: Vec<String>,
}

fn compile_command(compiler: &Path, extra: &[&str], case: &Case) -> Command {
    let mut command = Command::new(compiler);
    command
        .arg("-std=gnu17")
        .args(extra)
        .args(&case.flags)
        .args(["-c", "-o", "/dev/null"])
        .arg(&case.file)
        .stdout(Stdio::null());
    if let Some(cwd) = &case.cwd {
        command.current_dir(cwd);
    }
    command
}

fn time_compile(compiler: &Path, extra: &[&str], case: &Case) -> Option<f64> {
    let mut command = compile_command(compiler, extra, case);
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
    let mut command = compile_command(fcc, &[level], case);
    command.env("TIR_MEM_STATS", "1").stderr(Stdio::piped());
    let started = Instant::now();
    let output = command.output().ok()?;
    let ms = started.elapsed().as_secs_f64() * 1e3;
    if !output.status.success() {
        return None;
    }
    let peak_kb = parse_peak_kb(&String::from_utf8_lossy(&output.stderr))?;
    Some((ms, peak_kb))
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

fn fetch_coremark(sh: &Shell, root: &Path) -> anyhow::Result<PathBuf> {
    let checkout = root.join(COREMARK_PATH);
    if !checkout.join(".git").is_dir() {
        fs::create_dir_all(&checkout)?;
        cmd!(sh, "git -C {checkout} init").run()?;
        cmd!(
            sh,
            "git -C {checkout} remote add origin {COREMARK_REPOSITORY}"
        )
        .run()?;
    }
    cmd!(
        sh,
        "git -C {checkout} fetch --depth 1 origin {COREMARK_REVISION}"
    )
    .run()?;
    cmd!(sh, "git -C {checkout} checkout --detach FETCH_HEAD").run()?;
    Ok(checkout)
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

fn shared_sums(level: Level, baseline: &Results, current: &Results) -> (f64, f64) {
    let before = baseline
        .samples
        .iter()
        .map(|sample| (sample.path.as_str(), level.fcc_ms(sample)))
        .collect::<HashMap<_, _>>();
    current
        .samples
        .iter()
        .filter_map(|sample| {
            before
                .get(sample.path.as_str())
                // A baseline with no time at this level says nothing about it.
                .filter(|ms| **ms > 0.0)
                .map(|ms| (ms, level.fcc_ms(sample)))
        })
        .fold((0.0, 0.0), |(b, a), (before, after)| {
            (b + before, a + after)
        })
}

fn shared_peak_sums(level: Level, baseline: &Results, current: &Results) -> (f64, f64) {
    shared_peaks(level, baseline, current).fold((0.0, 0.0), |(b, a), (_, before, after)| {
        (b + before as f64, a + after as f64)
    })
}

fn shared_peaks<'a>(
    level: Level,
    baseline: &Results,
    current: &'a Results,
) -> impl Iterator<Item = (&'a str, u64, u64)> {
    let before = baseline
        .samples
        .iter()
        .map(|sample| (sample.path.as_str(), level.fcc_peak_kb(sample)))
        .collect::<HashMap<_, _>>();
    current
        .samples
        .iter()
        .filter_map(move |sample| {
            before
                .get(sample.path.as_str())
                // A baseline with no peak at this level says nothing about it.
                .filter(|kb| **kb > 0)
                .map(|kb| (sample.path.as_str(), *kb, level.fcc_peak_kb(sample)))
        })
        .collect::<Vec<_>>()
        .into_iter()
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
    let mut worst = shared_peaks(level, baseline, current).collect::<Vec<_>>();
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
            "fcc bench: peak RSS at {} regressed against the baseline",
            level.flag()
        );
    }
    if let Some((path, before, after)) = worst
        .first()
        .filter(|(_, before, after)| *after as f64 > *before as f64 * RSS_CASE_THRESHOLD)
    {
        anyhow::bail!(
            "fcc bench: peak RSS at {} regressed on {path}: {before} kB -> {after} kB",
            level.flag()
        );
    }
    Ok(())
}

fn report(results: &Results) -> String {
    let mut out = format!("fcc bench: {} cases\n", results.samples.len());
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
