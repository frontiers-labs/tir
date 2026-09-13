mod command;
pub(crate) mod config;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use command::{execute, expand, measure, Measurement, Variables};
use serde::{Deserialize, Serialize};

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
    samples: Vec<Sample>,
}

pub fn run(root: &Path, task: Task) -> anyhow::Result<()> {
    let (mode, options) = match task {
        Task::Compile(options) => ("compile", options),
        Task::Run(options) => ("run", options),
    };
    let root = root.canonicalize()?;
    let selected = config::discover(
        &root,
        options.suite.as_deref(),
        options.package.as_deref(),
        &options.bench,
    )?;
    if options.list {
        for selection in selected {
            println!("{}/{}", selection.suite.suite.package, selection.name);
        }
        return Ok(());
    }
    let cache = root.join("target/extbench/sources");
    fs::create_dir_all(&cache)?;
    let scratch = tempfile::tempdir_in(root.join("target/extbench"))?;
    let mut built = BTreeSet::new();
    let mut results = Results {
        mode: mode.into(),
        host: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        samples: Vec::new(),
    };
    for selection in &selected {
        let compilers = selection
            .suite
            .compiler
            .iter()
            .filter(|compiler| {
                options
                    .compiler
                    .as_ref()
                    .is_none_or(|name| name == &compiler.name)
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !compilers.is_empty(),
            "no compilers selected for {}",
            selection.suite.suite.package
        );
        let prepared = config::prepare(selection, &cache)?;
        for compiler in compilers {
            let mut variables = Variables::from([
                ("{root}", vec![root.display().to_string()]),
                ("{flags}", prepared.config.flags.clone()),
                ("{link_flags}", prepared.config.link_flags.clone()),
            ]);
            if !options.no_build && !compiler.build.is_empty() {
                let argv = expand(&compiler.build, &variables)?;
                if built.insert(argv.clone()) {
                    execute(&argv, &root, &compiler.env)?;
                }
            }
            for level in &prepared.config.levels {
                variables.insert("{level}", vec![level.clone()]);
                let mut objects = Vec::new();
                for (index, source) in prepared.sources.iter().enumerate() {
                    let object = scratch.path().join(format!("{index}.o"));
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
                        source: source
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
                        let executable = scratch.path().join("benchmark");
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
                        let mut argv = vec![executable.display().to_string()];
                        argv.extend(prepared.config.args.clone());
                        let measurement = measure(&argv, &prepared.directory, None)?;
                        let source = if prepared.config.separate {
                            prepared.sources[index]
                                .strip_prefix(&prepared.directory)?
                                .display()
                                .to_string()
                        } else {
                            "run".into()
                        };
                        let sample = Sample {
                            package: selection.suite.suite.package.clone(),
                            benchmark: selection.name.clone(),
                            compiler: compiler.name.clone(),
                            level: level.clone(),
                            source,
                            measurement,
                        };
                        report(&sample);
                        results.samples.push(sample);
                    }
                }
            }
        }
    }
    if let Some(output) = &options.output {
        fs::write(output, serde_json::to_string_pretty(&results)?)?;
    }
    if let Some(path) = &options.baseline {
        let baseline: Results = serde_json::from_str(&fs::read_to_string(path)?)?;
        compare(&baseline, &results)?;
    }
    Ok(())
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

fn compare(baseline: &Results, current: &Results) -> anyhow::Result<()> {
    use std::collections::BTreeMap;

    anyhow::ensure!(
        baseline.mode == current.mode,
        "baseline command differs from current command"
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
        println!("{package} {compiler} {level} baseline wall_ms={old_ms:.3}->{ms:.3} peak_rss_sum_kb={old_rss}->{rss}");
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

/// GCC and Clang compile times are runner weather; only FCC is this tree.
fn gates_compiler(compiler: &str) -> bool {
    compiler == "fcc"
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
            },
        }
    }

    fn results(samples: Vec<Sample>) -> Results {
        Results {
            mode: "compile".into(),
            host: "x86_64-linux".into(),
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
