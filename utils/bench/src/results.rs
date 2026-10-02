use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Engine, Options};

pub type Metrics = BTreeMap<String, f64>;

/// Metrics the summary exports, as key, label, unit and description, in the
/// order benchboard lists them. Other sampled values stay in `results.json`.
const METRICS: [(&str, &str, &str, &str); 15] = [
    (
        "latency",
        "Wall time",
        "ns",
        "Elapsed time of one execution. Median of the samples, with their median absolute deviation as the spread.",
    ),
    (
        "peak_process_rss_bytes",
        "Peak memory",
        "bytes",
        "Largest resident set size of the measured process. Maximum over the samples.",
    ),
    (
        "Ir",
        "Instructions",
        "count",
        "Instructions executed under Cachegrind. Repeats to about 0.2% on shared runners, so regressions are gated on it.",
    ),
    (
        "Dr",
        "Data reads",
        "count",
        "Memory reads executed under Cachegrind.",
    ),
    (
        "Dw",
        "Data writes",
        "count",
        "Memory writes executed under Cachegrind.",
    ),
    (
        "I1mr",
        "L1 instruction misses",
        "count",
        "Instruction fetches that miss Cachegrind's simulated first-level cache.",
    ),
    (
        "ILmr",
        "Last-level instruction misses",
        "count",
        "Instruction fetches that miss Cachegrind's simulated last-level cache.",
    ),
    (
        "D1mr",
        "L1 data read misses",
        "count",
        "Memory reads that miss Cachegrind's simulated first-level cache.",
    ),
    (
        "D1mw",
        "L1 data write misses",
        "count",
        "Memory writes that miss Cachegrind's simulated first-level cache.",
    ),
    (
        "DLmr",
        "Last-level data read misses",
        "count",
        "Memory reads that miss Cachegrind's simulated last-level cache.",
    ),
    (
        "DLmw",
        "Last-level data write misses",
        "count",
        "Memory writes that miss Cachegrind's simulated last-level cache.",
    ),
    (
        "Bc",
        "Conditional branches",
        "count",
        "Conditional branches executed under Cachegrind.",
    ),
    (
        "Bcm",
        "Conditional branch mispredictions",
        "count",
        "Conditional branches that Cachegrind's simulated predictor gets wrong.",
    ),
    (
        "Bi",
        "Indirect branches",
        "count",
        "Indirect branches executed under Cachegrind.",
    ),
    (
        "Bim",
        "Indirect branch mispredictions",
        "count",
        "Indirect branches that Cachegrind's simulated predictor gets wrong.",
    ),
];

/// Places a case among the variants of one benchmark, such as the compilers
/// that build the same file. Benchboard compares the references with the subject.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Variant {
    /// Name shared by every variant of the benchmark.
    pub benchmark: String,
    /// Heading that benchboard lists the benchmark under.
    pub group: String,
    pub variant: String,
    /// The variant under test. The other variants are its references.
    pub subject: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub metadata: Value,
    pub gate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<Variant>,
    pub samples: Vec<Metrics>,
    pub summary: Metrics,
}

#[derive(Serialize, Deserialize)]
pub struct Results {
    pub schema: u32,
    pub namespace: String,
    pub engine: Engine,
    pub environment: Value,
    pub provenance: Value,
    pub status: String,
    pub cases: Vec<Record>,
}

pub fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n.is_multiple_of(2) {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    } else {
        values[n / 2]
    }
}

pub fn summarize(samples: &[Metrics]) -> Result<Metrics> {
    ensure!(!samples.is_empty(), "no measurement samples");
    let keys = samples[0].keys().collect::<Vec<_>>();
    ensure!(
        samples.iter().all(|s| s.keys().collect::<Vec<_>>() == keys),
        "sample metric sets differ"
    );
    let mut result = Metrics::new();
    for key in keys {
        let mut values = samples.iter().map(|s| s[key]).collect::<Vec<_>>();
        ensure!(
            values.iter().all(|v| v.is_finite() && *v >= 0.0),
            "invalid metric {key}"
        );
        let value = if key == "peak_process_rss_bytes" {
            values.iter().copied().fold(0.0, f64::max)
        } else {
            median(&mut values)
        };
        result.insert(key.clone(), value);
        if key == "latency" {
            let mut deviations = values.iter().map(|v| (v - value).abs()).collect::<Vec<_>>();
            result.insert("latency_mad_ns".into(), median(&mut deviations));
        }
    }
    Ok(result)
}

/// Print one case the way Criterion prints a benchmark: its name, then labelled values.
pub fn report(id: &str, summary: &Metrics, samples: usize) {
    println!("{id}");
    if let Some(instructions) = summary.get("Ir") {
        println!("{:24}instructions: {instructions}", "");
    }
    if let Some(latency) = summary.get("latency") {
        println!(
            "{:24}time:   {} (median of {samples}, MAD {})",
            "",
            duration(*latency),
            duration(summary["latency_mad_ns"])
        );
    }
    if let Some(rss) = summary.get("peak_process_rss_bytes") {
        println!("{:24}rss:    {:.1} MiB peak", "", rss / (1024.0 * 1024.0));
    }
}

fn duration(nanoseconds: f64) -> String {
    let (value, unit) = match nanoseconds {
        ns if ns < 1e3 => (ns, "ns"),
        ns if ns < 1e6 => (ns / 1e3, "µs"),
        ns if ns < 1e9 => (ns / 1e6, "ms"),
        ns => (ns / 1e9, "s"),
    };
    // Four significant digits, as Criterion prints them.
    let decimals = 3 - (value.max(1.0).log10().floor() as usize).min(3);
    format!("{value:.decimals$} {unit}")
}

impl Results {
    pub fn save(&self, directory: &Path) -> Result<()> {
        let reported = |key: &str| self.cases.iter().any(|case| case.summary.contains_key(key));
        let metrics: Vec<_> = METRICS
            .iter()
            .filter(|(key, ..)| reported(key))
            .map(|(key, label, unit, description)| {
                json!({"key": key, "label": label, "unit": unit, "description": description})
            })
            .collect();
        let results: BTreeMap<_, BTreeMap<_, _>> = self
            .cases
            .iter()
            .map(|case| {
                (
                    &case.id,
                    METRICS
                        .iter()
                        .filter_map(|(key, ..)| measure(case, key))
                        .collect(),
                )
            })
            .collect();
        let variants: BTreeMap<_, _> = self
            .cases
            .iter()
            .filter_map(|case| Some((&case.id, case.variant.as_ref()?)))
            .collect();
        let summary = json!({"metrics": metrics, "results": results, "variants": variants});
        std::fs::write(
            directory.join("results.json"),
            serde_json::to_vec_pretty(self)?,
        )?;
        std::fs::write(
            directory.join("summary.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;
        Ok(())
    }

    pub fn compare(&self, old: &Self, options: &Options) -> Result<()> {
        ensure!(old.status == "complete", "baseline is incomplete");
        ensure!(
            self.schema == old.schema
                && self.namespace == old.namespace
                && self.engine == old.engine,
            "baseline measurement contract differs"
        );
        ensure!(
            environment_contract(&self.environment, self.engine)
                == environment_contract(&old.environment, old.engine),
            "baseline environment differs"
        );
        let before: BTreeMap<_, _> = old.cases.iter().map(|c| (&c.id, c)).collect();
        let after: BTreeMap<_, _> = self.cases.iter().map(|c| (&c.id, c)).collect();
        ensure!(
            before.len() == old.cases.len() && after.len() == self.cases.len(),
            "duplicate benchmark ID"
        );
        ensure!(
            before.keys().eq(after.keys()),
            "baseline case inventory differs"
        );
        let mut regressions = Vec::new();
        for (id, case) in after {
            let prior = before[id];
            ensure!(
                workload_contract(&case.metadata) == workload_contract(&prior.metadata),
                "baseline workload differs for {id}"
            );
            ensure!(
                case.summary.keys().eq(prior.summary.keys()),
                "baseline metric set differs for {id}"
            );
            ensure!(
                case.gate == prior.gate,
                "baseline gate role differs for {id}"
            );
            if !case.gate {
                continue;
            }
            for metric in ["latency", "Ir", "peak_process_rss_bytes"] {
                let Some(&value) = case.summary.get(metric) else {
                    continue;
                };
                let baseline = prior.summary[metric];
                let threshold = if metric == "peak_process_rss_bytes" {
                    options.rss_threshold
                } else {
                    options.threshold
                };
                if value > baseline * (1.0 + threshold / 100.0) {
                    regressions.push(format!(
                        "{id} {metric}: {baseline:.3} -> {value:.3} exceeds {threshold}%"
                    ));
                }
            }
        }
        ensure!(
            regressions.is_empty(),
            "benchmark regressions:\n{}",
            regressions.join("\n")
        );
        Ok(())
    }
}

/// One summary value. Wall time carries its deviation as a range.
fn measure<'a>(case: &Record, key: &'a str) -> Option<(&'a str, Value)> {
    let value = *case.summary.get(key)?;
    let mut measure = json!({"value": value});
    if key == "latency" {
        let deviation = case.summary["latency_mad_ns"];
        measure["lower_value"] = json!(value - deviation);
        measure["upper_value"] = json!(value + deviation);
    }
    Some((key, measure))
}

fn workload_contract(metadata: &Value) -> Value {
    let mut contract = metadata.clone();
    if let Some(object) = contract.as_object_mut() {
        object.remove("provenance");
        if let Some(workload) = object.get_mut("workload").and_then(Value::as_object_mut) {
            workload.remove("provenance");
        }
    }
    contract
}

fn environment_contract(environment: &Value, engine: Engine) -> Value {
    let mut contract = environment.clone();
    if engine == Engine::Cachegrind
        && let Some(object) = contract.as_object_mut()
    {
        // Counts use a fixed simulated cache; physical scheduling is not the model.
        for key in ["hostname", "cpu", "allowed_cpus", "applied_cpus"] {
            object.remove(key);
        }
    }
    contract
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn samples_preserve_median_and_peak_semantics() {
        let samples = [
            json!({"latency": 2., "peak_process_rss_bytes": 10.}),
            json!({"latency": 4., "peak_process_rss_bytes": 8.}),
        ]
        .map(|v| serde_json::from_value(v).unwrap());
        let summary = summarize(&samples).unwrap();
        assert_eq!(summary["latency"], 3.);
        assert_eq!(summary["latency_mad_ns"], 1.);
        assert_eq!(summary["peak_process_rss_bytes"], 10.);
    }
}
