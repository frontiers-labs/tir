use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Options;
use crate::statistics::{Distribution, Modality};

pub type Metrics = BTreeMap<String, f64>;

/// Metrics the summary exports, as key, label, unit and description, in the
/// order benchboard lists them. Other sampled values stay in `results.json`.
const METRICS: [(&str, &str, &str, &str); 2] = [
    (
        "latency",
        "Wall time",
        "ns",
        "Elapsed time of one execution. Median of the samples, with the quartiles as the spread. Lower is better.",
    ),
    (
        "peak_process_rss_bytes",
        "Peak memory",
        "bytes",
        "Largest resident set size of the measured process. Maximum over the samples. Lower is better.",
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
    pub environment: Value,
    pub provenance: Value,
    pub status: String,
    pub cases: Vec<Record>,
}

fn median(values: &mut [f64]) -> f64 {
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
        match key.as_str() {
            "peak_process_rss_bytes" => {
                result.insert(key.clone(), values.iter().copied().fold(0.0, f64::max));
            }
            "latency" => {
                let distribution = Distribution::of(&values);
                result.extend([
                    ("latency".into(), distribution.median),
                    ("latency_lower_quartile".into(), distribution.lower_quartile),
                    ("latency_upper_quartile".into(), distribution.upper_quartile),
                    ("latency_outliers".into(), distribution.outliers as f64),
                    ("latency_mvalue".into(), distribution.mvalue),
                ]);
            }
            _ => {
                result.insert(key.clone(), median(&mut values));
            }
        }
    }
    Ok(result)
}

/// Print one case the way Criterion prints a benchmark: its name, then labelled values.
pub fn report(id: &str, summary: &Metrics, samples: usize) {
    println!("{id}");
    if let Some(latency) = summary.get("latency") {
        println!(
            "{:24}time:   {} (median of {samples}, quartiles {} to {})",
            "",
            duration(*latency),
            duration(summary["latency_lower_quartile"]),
            duration(summary["latency_upper_quartile"]),
        );
        let modality = Modality::of(summary["latency_mvalue"]);
        let outliers = summary["latency_outliers"];
        if modality != Modality::Unimodal || outliers > 0.0 {
            println!("{:24}shape:  {}, {outliers} outliers", "", modality.name());
        }
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
        // One record on benchboard holds every architecture, so ids and groups
        // name theirs. The CPU model tells runners of one architecture apart.
        let architecture = self.environment["architecture"]
            .as_str()
            .unwrap_or("unknown");
        let cpu = self.environment["cpu_model"].as_str().unwrap_or("unknown");
        let id = |case: &Record| format!("{architecture}/{}", case.id);
        let results: BTreeMap<_, BTreeMap<_, _>> = self
            .cases
            .iter()
            .map(|case| {
                (
                    id(case),
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
            .filter_map(|case| {
                let variant = case.variant.as_ref()?;
                let group = format!("{} on {architecture}", variant.group);
                Some((
                    id(case),
                    Variant {
                        group,
                        ..variant.clone()
                    },
                ))
            })
            .collect();
        let summary = json!({
            "metrics": metrics,
            "environment": format!("{architecture} {cpu}"),
            "results": results,
            "variants": variants,
        });
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
            self.schema == old.schema && self.namespace == old.namespace,
            "baseline measurement contract differs"
        );
        ensure!(
            self.environment == old.environment,
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
            for metric in ["latency", "peak_process_rss_bytes"] {
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

fn measure<'a>(case: &Record, key: &'a str) -> Option<(&'a str, Value)> {
    let value = *case.summary.get(key)?;
    let mut measure = json!({"value": value});
    if key == "latency" {
        measure["lower_value"] = json!(case.summary["latency_lower_quartile"]);
        measure["upper_value"] = json!(case.summary["latency_upper_quartile"]);
        measure["samples"] = case.samples.iter().map(|sample| sample[key]).collect();
        measure["modality"] = json!(Modality::of(case.summary["latency_mvalue"]).name());
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
        assert_eq!(summary["latency_lower_quartile"], 2.5);
        assert_eq!(summary["peak_process_rss_bytes"], 10.);
    }
}
