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
const SLOWEST_SHOWN: usize = 10;

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

#[derive(Serialize, Deserialize)]
pub struct Sample {
    pub path: String,
    pub fcc_ms: f64,
    pub gcc_ms: f64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Results {
    pub samples: Vec<Sample>,
}

/// Times fcc and `gcc -O0` on every passing torture execute case and the
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
        let fcc_ms = time_compile(&fcc, &[], case);
        let gcc_ms = time_compile(Path::new("gcc"), &["-O0"], case);
        match (fcc_ms, gcc_ms) {
            (Some(fcc_ms), Some(gcc_ms)) => results.samples.push(Sample {
                path: case.label.clone(),
                fcc_ms,
                gcc_ms,
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
    if let Some(baseline) = &options.baseline {
        let baseline: Results = serde_json::from_str(&fs::read_to_string(baseline)?)?;
        let (before, after) = shared_sums(&baseline, &results);
        println!(
            "fcc sum vs baseline over shared cases: {:.1} s -> {:.1} s ({:+.1} %)",
            before / 1e3,
            after / 1e3,
            (after / before - 1.0) * 100.0
        );
        if after > before * REGRESSION_THRESHOLD {
            anyhow::bail!("fcc bench: compile time regressed against the baseline");
        }
    }
    if let Some(output) = &options.output {
        fs::write(output, serde_json::to_string_pretty(&results)?)?;
    }
    Ok(())
}

struct Case {
    label: String,
    file: PathBuf,
    cwd: Option<PathBuf>,
    flags: Vec<String>,
}

fn time_compile(compiler: &Path, extra: &[&str], case: &Case) -> Option<f64> {
    let mut command = Command::new(compiler);
    command
        .arg("-std=gnu17")
        .args(extra)
        .args(&case.flags)
        .args(["-c", "-o", "/dev/null"])
        .arg(&case.file)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = &case.cwd {
        command.current_dir(cwd);
    }
    let started = Instant::now();
    let success = command.status().is_ok_and(|status| status.success());
    success.then(|| started.elapsed().as_secs_f64() * 1e3)
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

fn fcc_sum(results: &Results) -> f64 {
    results.samples.iter().map(|sample| sample.fcc_ms).sum()
}

fn median_ratio(results: &Results) -> f64 {
    let mut ratios = results
        .samples
        .iter()
        .map(|sample| sample.fcc_ms / sample.gcc_ms)
        .collect::<Vec<_>>();
    ratios.sort_by(|a, b| a.total_cmp(b));
    match ratios.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => ratios[n / 2],
        n => (ratios[n / 2 - 1] + ratios[n / 2]) / 2.0,
    }
}

fn shared_sums(baseline: &Results, current: &Results) -> (f64, f64) {
    let before = baseline
        .samples
        .iter()
        .map(|sample| (sample.path.as_str(), sample.fcc_ms))
        .collect::<HashMap<_, _>>();
    current
        .samples
        .iter()
        .filter_map(|sample| {
            before
                .get(sample.path.as_str())
                .map(|ms| (ms, sample.fcc_ms))
        })
        .fold((0.0, 0.0), |(b, a), (before, after)| {
            (b + before, a + after)
        })
}

fn report(results: &Results) -> String {
    let gcc_sum: f64 = results.samples.iter().map(|sample| sample.gcc_ms).sum();
    let mut out = format!(
        "fcc bench: {} cases, fcc {:.1} s, gcc -O0 {:.1} s, median ratio {:.1}x\n",
        results.samples.len(),
        fcc_sum(results) / 1e3,
        gcc_sum / 1e3,
        median_ratio(results)
    );
    let mut slowest = results.samples.iter().collect::<Vec<_>>();
    slowest.sort_by(|a, b| b.fcc_ms.total_cmp(&a.fcc_ms));
    for sample in slowest.iter().take(SLOWEST_SHOWN) {
        out.push_str(&format!(
            "  {:>9.1} ms  {:>7.1} ms  {}\n",
            sample.fcc_ms, sample.gcc_ms, sample.path
        ));
    }
    out
}
