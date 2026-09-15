use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Instant;

use anyhow::Context;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::config::Compiler;

pub type Variables = BTreeMap<&'static str, Vec<String>>;

pub fn expand(template: &[String], variables: &Variables) -> anyhow::Result<Vec<String>> {
    let mut expanded = Vec::new();
    for argument in template {
        if let Some(values) = variables.get(argument.as_str()) {
            expanded.extend(values.clone());
            continue;
        }
        let mut argument = argument.clone();
        for (name, values) in variables {
            if values.len() == 1 {
                argument = argument.replace(name, &values[0]);
            }
        }
        anyhow::ensure!(
            !argument.contains('{') && !argument.contains('}'),
            "unknown or embedded list placeholder: {argument}"
        );
        expanded.push(argument);
    }
    anyhow::ensure!(!expanded.is_empty(), "empty command");
    Ok(expanded)
}

pub fn execute(
    argv: &[String],
    directory: &Path,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<Output> {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(directory)
        .envs(env)
        .output()
        .with_context(|| format!("starting {} in {}", argv[0], directory.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "{} failed ({}):\n{}{}",
        argv[0],
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RawMeasurement {
    pub wall_ms: f64,
    pub peak_rss_kb: u64,
    pub metrics: BTreeMap<String, f64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Measurement {
    pub wall_ms: f64,
    pub peak_rss_kb: u64,
    pub metrics: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<RawMeasurement>,
}

pub fn measure(
    argv: &[String],
    directory: &Path,
    compiler: Option<&Compiler>,
) -> anyhow::Result<Measurement> {
    Ok(measure_output(argv, directory, compiler)?.0)
}

pub fn measure_output(
    argv: &[String],
    directory: &Path,
    compiler: Option<&Compiler>,
) -> anyhow::Result<(Measurement, Output)> {
    let mut timed = vec!["/usr/bin/time".to_string()];
    if cfg!(target_os = "macos") {
        timed.push("-l".into());
    } else {
        anyhow::ensure!(
            cfg!(target_os = "linux"),
            "extbench memory measurement requires Linux or macOS"
        );
        timed.extend(["-f".into(), "extbench-rss-kb=%M".into()]);
    }
    timed.extend_from_slice(argv);
    let empty = BTreeMap::new();
    let started = Instant::now();
    let output = execute(&timed, directory, compiler.map_or(&empty, |c| &c.env))?;
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let peak_rss_kb = if cfg!(target_os = "macos") {
        stderr
            .lines()
            .find_map(|line| line.trim().strip_suffix(" maximum resident set size"))
            .context("missing peak RSS")?
            .trim()
            .parse::<u64>()?
            / 1024
    } else {
        stderr
            .lines()
            .find_map(|line| line.strip_prefix("extbench-rss-kb="))
            .context("missing peak RSS")?
            .parse()?
    };
    let mut measured_metrics = BTreeMap::new();
    if let Some(compiler) = compiler {
        for (name, pattern) in &compiler.metrics {
            let regex = Regex::new(pattern)?;
            let values = regex
                .captures_iter(&stderr)
                .map(|capture| {
                    let value: f64 = capture
                        .get(1)
                        .context("metric requires one capture group")?
                        .as_str()
                        .parse()?;
                    anyhow::ensure!(value.is_finite() && value >= 0.0, "invalid metric {name}");
                    Ok(value)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            anyhow::ensure!(
                !values.is_empty(),
                "{} did not report {name}:\n{stderr}",
                compiler.name
            );
            measured_metrics.insert(name.clone(), values.iter().sum());
        }
    }
    Ok((
        Measurement {
            wall_ms,
            peak_rss_kb,
            metrics: measured_metrics,
            runs: Vec::new(),
        },
        output,
    ))
}
