use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Engine, Options};

pub type Metrics = BTreeMap<String, f64>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub metadata: Value,
    pub gate: bool,
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

impl Results {
    pub fn save(&self, directory: &Path) -> Result<()> {
        let bmf: BTreeMap<_, BTreeMap<_, _>> = self
            .cases
            .iter()
            .map(|case| {
                (
                    &case.id,
                    case.summary
                        .iter()
                        .map(|(k, v)| (k, json!({"value": v})))
                        .collect(),
                )
            })
            .collect();
        std::fs::write(
            directory.join("results.json"),
            serde_json::to_vec_pretty(self)?,
        )?;
        std::fs::write(
            directory.join("summary.bmf.json"),
            serde_json::to_vec_pretty(&bmf)?,
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
