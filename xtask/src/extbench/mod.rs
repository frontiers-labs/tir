mod command;
pub(crate) mod config;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use command::{execute, expand, measure, measure_output, Measurement, RawMeasurement, Variables};
use config::Input;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(clap::Subcommand)]
pub enum Task {
    /// Measure compilation time and memory for each configured compiler.
    Compile(Options),
    /// Build and time benchmarks on the current host.
    Run(Options),
}

#[derive(clap::Args)]
pub struct Options {
    /// Select a target package.
    #[arg(short, long)]
    package: Option<String>,
    /// Select benchmark directories by glob.
    #[arg(short, long, default_value = "*")]
    bench: String,
    /// Select a configured compiler by name.
    #[arg(long)]
    compiler: Option<String>,
    /// Select source files or Clang-produced LLVM IR as compiler input.
    #[arg(long, value_enum, default_value_t)]
    input: Input,
    /// Use a specific bench_suite.toml instead of workspace discovery.
    #[arg(long)]
    suite: Option<PathBuf>,
    /// List matching benchmarks without building or fetching sources.
    #[arg(long)]
    list: bool,
    /// Reuse previously built compilers.
    #[arg(long)]
    no_build: bool,
    /// Write machine-readable samples to this file.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Compare time and RSS against a previous run of the same command.
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Keep inputs, outputs, executables, and run output in a new directory.
    #[arg(long)]
    artifacts: Option<PathBuf>,
    /// Number of interleaved executions in run mode.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    runs: u32,
    /// Select one optimization level, such as -O2.
    #[arg(long, allow_hyphen_values = true)]
    level: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Sample {
    package: String,
    benchmark: String,
    compiler: String,
    level: String,
    source: String,
    #[serde(flatten)]
    measurement: Measurement,
}

#[derive(Serialize, Deserialize)]
struct Results {
    mode: String,
    host: String,
    #[serde(default)]
    input: Input,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    producer: Option<Producer>,
    #[serde(default)]
    workload: String,
    samples: Vec<Sample>,
}

fn create_artifacts(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        !path.exists(),
        "artifact directory already exists: {}",
        path.display()
    );
    fs::create_dir_all(path)?;
    Ok(())
}

fn summarize(measurements: Vec<Measurement>) -> Measurement {
    let mut wall_times = measurements.iter().map(|m| m.wall_ms).collect::<Vec<_>>();
    wall_times.sort_by(f64::total_cmp);
    let middle = wall_times.len() / 2;
    let wall_ms = if wall_times.len() % 2 == 0 {
        (wall_times[middle - 1] + wall_times[middle]) / 2.0
    } else {
        wall_times[middle]
    };
    let peak_rss_kb = measurements.iter().map(|m| m.peak_rss_kb).max().unwrap();
    let metrics = BTreeMap::new();
    let runs = measurements
        .into_iter()
        .map(|m| RawMeasurement {
            wall_ms: m.wall_ms,
            peak_rss_kb: m.peak_rss_kb,
            metrics: m.metrics,
        })
        .collect();
    Measurement {
        wall_ms,
        peak_rss_kb,
        metrics,
        runs,
    }
}

struct RunJob {
    compiler: String,
    executable: PathBuf,
    source: String,
    measurements: Vec<Measurement>,
    directory: PathBuf,
}

fn measure_jobs(
    jobs: &mut [RunJob],
    runs: u32,
    args: &[String],
    verify: &[String],
    benchmark: &Path,
    workdir: &Path,
) -> anyhow::Result<()> {
    for round in 0..runs {
        for offset in 0..jobs.len() {
            let index = (round as usize + offset) % jobs.len();
            let job = &mut jobs[index];
            let mut argv = vec![job.executable.display().to_string()];
            argv.extend_from_slice(args);
            let (measurement, output) = measure_output(&argv, workdir, None)?;
            let stdout = job.directory.join(format!("run-{round}.stdout"));
            let stderr = job.directory.join(format!("run-{round}.stderr"));
            fs::write(&stdout, &output.stdout)?;
            fs::write(stderr, &output.stderr)?;
            if !verify.is_empty() {
                let variables = Variables::from([
                    ("{stdout}", vec![stdout.display().to_string()]),
                    ("{args}", args.to_vec()),
                    ("{benchmark}", vec![benchmark.display().to_string()]),
                ]);
                execute(&expand(verify, &variables)?, workdir, &Default::default())?;
            }
            job.measurements.push(measurement);
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize, PartialEq)]
struct Producer {
    version: String,
    settings: Vec<String>,
    digest: String,
}

fn select_benchmarks(root: &Path, options: &Options) -> anyhow::Result<Vec<config::Selection>> {
    let selected = config::discover(
        root,
        options.suite.as_deref(),
        options.package.as_deref(),
        &options.bench,
    )?
    .into_iter()
    .map(|selection| {
        Ok((
            config::supports_input(&selection, options.input)?,
            selection,
        ))
    })
    .collect::<anyhow::Result<Vec<_>>>()?
    .into_iter()
    .filter_map(|(supported, selection)| supported.then_some(selection))
    .collect::<Vec<_>>();
    anyhow::ensure!(
        !selected.is_empty(),
        "no benchmarks support {} input",
        match options.input {
            Input::Source => "source",
            Input::Llvm => "llvm",
        }
    );
    Ok(selected)
}

fn finish_results(
    mut results: Results,
    options: &Options,
    producer_digest: Sha256,
    workload_digest: Sha256,
) -> anyhow::Result<()> {
    if let Some(producer) = &mut results.producer {
        producer.digest = format!("sha256:{:x}", producer_digest.result());
    }
    results.workload = format!("sha256:{:x}", workload_digest.result());
    if let Some(output) = &options.output {
        fs::write(output, serde_json::to_string_pretty(&results)?)?;
    }
    if let Some(path) = &options.baseline {
        let baseline: Results = serde_json::from_str(&fs::read_to_string(path)?)?;
        compare(&baseline, &results)?;
    }
    Ok(())
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.input((value.len() as u64).to_le_bytes());
    digest.input(value);
}

pub fn run(root: &Path, task: Task) -> anyhow::Result<()> {
    let (mode, options) = match task {
        Task::Compile(options) => ("compile", options),
        Task::Run(options) => ("run", options),
    };
    let root = root.canonicalize()?;
    let selected = select_benchmarks(&root, &options)?;
    if options.list {
        for selection in selected {
            println!("{}/{}", selection.suite.suite.package, selection.name);
        }
        return Ok(());
    }
    let cache = root.join("target/extbench/sources");
    fs::create_dir_all(&cache)?;
    let scratch = tempfile::tempdir_in(root.join("target/extbench"))?;
    let artifact_root = match &options.artifacts {
        Some(path) => {
            create_artifacts(path)?;
            path.canonicalize()?
        }
        None => scratch.path().to_path_buf(),
    };
    let mut built = BTreeSet::new();
    let mut results = Results {
        mode: mode.into(),
        host: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        input: options.input,
        producer: None,
        workload: String::new(),
        samples: Vec::new(),
    };
    let mut producer_digest = Sha256::new();
    let mut workload_digest = Sha256::new();
    for selection in &selected {
        let compilers = selection
            .suite
            .compiler
            .iter()
            .filter(|compiler| {
                options
                    .compiler
                    .as_ref()
                    .map_or(!compiler.opt_in, |name| name == &compiler.name)
                    && options.input == compiler.input
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !compilers.is_empty(),
            "no compilers selected for {}",
            selection.suite.suite.package
        );
        let llvm = if options.input == Input::Llvm {
            Some(selection.suite.llvm.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} has no LLVM input preparation configured",
                    selection.suite.suite.package
                )
            })?)
        } else {
            None
        };
        if let Some(llvm) = llvm {
            let version = execute(
                &expand(&llvm.version, &Variables::new())?,
                &root,
                &Default::default(),
            )?;
            let version = String::from_utf8(version.stdout)?.trim().to_string();
            anyhow::ensure!(
                !version.is_empty(),
                "LLVM producer reported an empty version"
            );
            results.producer.get_or_insert_with(|| Producer {
                version,
                settings: llvm.prepare.clone(),
                digest: String::new(),
            });
        }
        let prepared = config::prepare(selection, &cache)?;
        digest_field(
            &mut workload_digest,
            selection.suite.suite.package.as_bytes(),
        );
        digest_field(&mut workload_digest, selection.name.as_bytes());
        for value in prepared
            .config
            .args
            .iter()
            .chain(prepared.config.flags.iter())
            .chain(prepared.config.link_flags.iter())
        {
            digest_field(&mut workload_digest, value.as_bytes());
        }
        for source in &prepared.sources {
            digest_field(&mut workload_digest, &fs::read(source)?);
        }
        if let Some(producer) = &mut results.producer {
            for level in &prepared.config.levels {
                producer.settings.push(format!(
                    "{}/{} level={} flags={:?}",
                    selection.suite.suite.package, selection.name, level, prepared.config.flags
                ));
            }
        }
        let levels = prepared
            .config
            .levels
            .iter()
            .filter(|level| options.level.as_ref().is_none_or(|wanted| wanted == *level))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !levels.is_empty(),
            "benchmark {}/{} has no selected level",
            selection.suite.suite.package,
            selection.name
        );
        for level in levels {
            let level_dir = artifact_root
                .join(&selection.suite.suite.package)
                .join(&selection.name)
                .join(level.trim_start_matches('-'));
            fs::create_dir_all(&level_dir)?;
            let mut inputs = Vec::new();
            for (index, source) in prepared.sources.iter().enumerate() {
                if let Some(llvm) = llvm {
                    let ir = level_dir.join(format!("input-{index}.ll"));
                    let variables = Variables::from([
                        ("{root}", vec![root.display().to_string()]),
                        ("{arch}", vec![host_arch().into()]),
                        ("{flags}", prepared.config.flags.clone()),
                        ("{link_flags}", prepared.config.link_flags.clone()),
                        ("{level}", vec![level.clone()]),
                        ("{source}", vec![source.display().to_string()]),
                        ("{output}", vec![ir.display().to_string()]),
                    ]);
                    execute(
                        &expand(&llvm.prepare, &variables)?,
                        &prepared.directory,
                        &Default::default(),
                    )?;
                    producer_digest.input(fs::read(&ir)?);
                    inputs.push(ir);
                } else {
                    inputs.push(source.clone());
                }
            }
            let mut jobs = Vec::new();
            for compiler in &compilers {
                let mut variables = Variables::from([
                    ("{root}", vec![root.display().to_string()]),
                    ("{arch}", vec![host_arch().into()]),
                    ("{flags}", prepared.config.flags.clone()),
                    ("{link_flags}", prepared.config.link_flags.clone()),
                ]);
                if !options.no_build && !compiler.build.is_empty() {
                    let argv = expand(&compiler.build, &variables)?;
                    if built.insert(argv.clone()) {
                        execute(&argv, &root, &compiler.env)?;
                    }
                }
                variables.insert("{level}", vec![level.clone()]);
                let mut objects = Vec::new();
                let compiler_dir = level_dir.join(&compiler.name);
                fs::create_dir_all(&compiler_dir)?;
                for (index, source) in inputs.iter().enumerate() {
                    let object = compiler_dir.join(format!("{index}.o"));
                    variables.insert("{source}", vec![source.display().to_string()]);
                    variables.insert("{output}", vec![object.display().to_string()]);
                    let argv = expand(&compiler.compile, &variables)?;
                    if mode == "run" {
                        execute(&argv, &prepared.directory, &compiler.env)?;
                        objects.push(object);
                        continue;
                    }
                    let measurement = measure(&argv, &prepared.directory, Some(compiler))?;
                    let sample = Sample {
                        package: selection.suite.suite.package.clone(),
                        benchmark: selection.name.clone(),
                        compiler: compiler.name.clone(),
                        level: level.clone(),
                        source: prepared.sources[index]
                            .strip_prefix(&prepared.directory)?
                            .display()
                            .to_string(),
                        measurement,
                    };
                    report(&sample);
                    results.samples.push(sample);
                }
                if mode == "run" {
                    let groups = if prepared.config.separate {
                        objects.chunks(1).collect::<Vec<_>>()
                    } else {
                        vec![objects.as_slice()]
                    };
                    for (index, group) in groups.iter().enumerate() {
                        let executable = compiler_dir.join(format!("benchmark-{index}"));
                        variables.insert(
                            "{objects}",
                            group.iter().map(|p| p.display().to_string()).collect(),
                        );
                        variables.insert("{output}", vec![executable.display().to_string()]);
                        execute(
                            &expand(&compiler.link, &variables)?,
                            &prepared.directory,
                            &compiler.env,
                        )?;
                        let source = if prepared.config.separate {
                            prepared.sources[index]
                                .strip_prefix(&prepared.directory)?
                                .display()
                                .to_string()
                        } else {
                            "run".into()
                        };
                        let directory = compiler_dir.join(format!("run-{index}"));
                        fs::create_dir_all(&directory)?;
                        jobs.push(RunJob {
                            compiler: compiler.name.clone(),
                            executable,
                            source,
                            measurements: Vec::new(),
                            directory,
                        });
                    }
                }
            }
            if mode == "run" {
                measure_jobs(
                    &mut jobs,
                    options.runs,
                    &prepared.config.args,
                    &prepared.config.verify,
                    &selection.directory,
                    &prepared.directory,
                )?;
                for job in jobs {
                    let sample = Sample {
                        package: selection.suite.suite.package.clone(),
                        benchmark: selection.name.clone(),
                        compiler: job.compiler,
                        level: level.clone(),
                        source: job.source,
                        measurement: summarize(job.measurements),
                    };
                    report(&sample);
                    results.samples.push(sample);
                }
            }
        }
    }
    finish_results(results, &options, producer_digest, workload_digest)
}

fn report(sample: &Sample) {
    print!(
        "{}/{} {} {} {} wall_ms={:.3} peak_rss_kb={}",
        sample.package,
        sample.benchmark,
        sample.compiler,
        sample.level,
        sample.source,
        sample.measurement.wall_ms,
        sample.measurement.peak_rss_kb
    );
    for (name, value) in &sample.measurement.metrics {
        print!(" {name}={value:.3}");
    }
    println!();
}

fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        arch => arch,
    }
}

fn compare(baseline: &Results, current: &Results) -> anyhow::Result<()> {
    use std::collections::BTreeMap;

    anyhow::ensure!(
        baseline.mode == current.mode,
        "baseline command differs from current command"
    );
    anyhow::ensure!(
        baseline.input == current.input,
        "baseline input differs from current input"
    );
    anyhow::ensure!(
        baseline.producer == current.producer,
        "baseline input producer differs from current input producer"
    );
    anyhow::ensure!(
        baseline.workload == current.workload,
        "baseline workload differs from current workload"
    );
    let key = |sample: &Sample| {
        (
            sample.package.clone(),
            sample.benchmark.clone(),
            sample.compiler.clone(),
            sample.level.clone(),
            sample.source.clone(),
        )
    };
    let before = baseline
        .samples
        .iter()
        .map(|sample| (key(sample), sample))
        .collect::<BTreeMap<_, _>>();
    let mut sums = BTreeMap::<_, (f64, f64, u64, u64)>::new();
    let mut regressions = Vec::new();
    for sample in &current.samples {
        let Some(old) = before.get(&key(sample)) else {
            continue;
        };
        let sum = sums
            .entry((&sample.package, &sample.compiler, &sample.level))
            .or_default();
        sum.0 += old.measurement.wall_ms;
        sum.1 += sample.measurement.wall_ms;
        sum.2 += old.measurement.peak_rss_kb;
        sum.3 += sample.measurement.peak_rss_kb;
        if gates_compiler(&sample.compiler)
            && sample.measurement.peak_rss_kb as f64 > old.measurement.peak_rss_kb as f64 * 1.35
        {
            regressions.push(format!(
                "{} {} {} peak RSS grew by more than 35%",
                sample.compiler, sample.benchmark, sample.source
            ));
        }
    }
    anyhow::ensure!(!sums.is_empty(), "baseline has no matching samples");
    for ((package, compiler, level), (old_ms, ms, old_rss, rss)) in sums {
        println!(
            "{package} {compiler} {level} baseline wall_ms={old_ms:.3}->{ms:.3} peak_rss_sum_kb={old_rss}->{rss}"
        );
        if !gates_compiler(compiler) {
            continue;
        }
        if ms > old_ms * 1.10 || rss as f64 > old_rss as f64 * 1.02 {
            regressions.push(format!("{package} {compiler} {level} time grew by more than 10% or peak RSS sum by more than 2%"));
        }
    }
    anyhow::ensure!(regressions.is_empty(), "{}", regressions.join("\n"));
    Ok(())
}

/// Gate the compilers built from this tree.
fn gates_compiler(compiler: &str) -> bool {
    matches!(compiler, "fcc" | "tir")
}

#[cfg(test)]
mod tests {
    use super::*;
    use command::Measurement;

    fn sample(compiler: &str, source: &str, wall_ms: f64, peak_rss_kb: u64) -> Sample {
        Sample {
            package: "fcc".into(),
            benchmark: "coremark".into(),
            compiler: compiler.into(),
            level: "-O0".into(),
            source: source.into(),
            measurement: Measurement {
                wall_ms,
                peak_rss_kb,
                metrics: Default::default(),
                runs: Vec::new(),
            },
        }
    }

    fn results(samples: Vec<Sample>) -> Results {
        Results {
            mode: "compile".into(),
            host: "x86_64-linux".into(),
            input: Input::Source,
            producer: None,
            workload: String::new(),
            samples,
        }
    }

    #[test]
    fn clang_wall_time_weather_is_not_a_regression() {
        let baseline = results(vec![sample("clang", "core_list_join.c", 377.0, 84_000)]);
        let current = results(vec![sample("clang", "core_list_join.c", 4276.0, 84_000)]);
        compare(&baseline, &current).unwrap();
    }

    #[test]
    fn fcc_wall_time_growth_is_a_regression() {
        let baseline = results(vec![sample("fcc", "core_list_join.c", 100.0, 20_000)]);
        let current = results(vec![sample("fcc", "core_list_join.c", 130.0, 20_000)]);
        assert!(compare(&baseline, &current).is_err());
    }

    #[test]
    fn tir_wall_time_growth_is_a_regression() {
        let baseline = results(vec![sample("tir", "core_list_join.c", 100.0, 20_000)]);
        let current = results(vec![sample("tir", "core_list_join.c", 130.0, 20_000)]);
        assert!(compare(&baseline, &current).is_err());
    }

    #[test]
    fn clang_peak_rss_weather_is_not_a_regression() {
        let baseline = results(vec![sample("clang", "core_list_join.c", 10.0, 10_000)]);
        let current = results(vec![sample("clang", "core_list_join.c", 10.0, 20_000)]);
        compare(&baseline, &current).unwrap();
    }

    #[test]
    fn fcc_peak_rss_growth_is_a_regression() {
        let baseline = results(vec![sample("fcc", "core_list_join.c", 10.0, 10_000)]);
        let current = results(vec![sample("fcc", "core_list_join.c", 10.0, 20_000)]);
        assert!(compare(&baseline, &current).is_err());
    }
}
