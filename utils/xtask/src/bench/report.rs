use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::Value;

type Snapshot = BTreeMap<PathBuf, (SystemTime, u64)>;

pub(super) fn snapshot(root: &Path) -> Result<Snapshot> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            files.extend(snapshot(&entry.path())?);
        } else if entry
            .path()
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|p| p == "new")
            && matches!(
                entry.file_name().to_str(),
                Some("sample.json" | "bencher.json")
            )
        {
            let metadata = entry.metadata()?;
            files.insert(entry.path(), (metadata.modified()?, metadata.len()));
        }
    }
    Ok(files)
}

struct Row {
    name: String,
    samples: Vec<f64>,
    metrics: BTreeMap<String, f64>,
}

impl Row {
    fn cells(mut self, metrics: &BTreeSet<String>) -> Vec<String> {
        self.samples.sort_by(f64::total_cmp);
        let count = self.samples.len();
        let mut cells = vec![self.name, count.to_string()];
        if count == 0 {
            cells.extend(std::iter::repeat_n(String::new(), 6));
        } else {
            let min = self.samples[0];
            let max = self.samples[count - 1];
            let median = (self.samples[(count - 1) / 2] + self.samples[count / 2]) / 2.0;
            let mean = self.samples.iter().sum::<f64>() / count as f64;
            let variance = if count > 1 {
                self.samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (count - 1) as f64
            } else {
                0.0
            };
            cells.extend(
                [min, max, median, mean, variance.sqrt(), 1e9 / median]
                    .map(|value| value.to_string()),
            );
        }
        cells.extend(metrics.iter().map(|name| {
            self.metrics
                .get(name)
                .map(ToString::to_string)
                .unwrap_or_default()
        }));
        cells
    }
}

#[derive(Deserialize)]
struct Sample {
    iters: Vec<f64>,
    times: Vec<f64>,
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path)?).with_context(|| format!("reading {}", path.display()))
}

fn read_rows(path: &Path) -> Result<Vec<Row>> {
    let json = read_json(path)?;
    if path.file_name().is_some_and(|name| name == "bencher.json") {
        return json
            .as_object()
            .context("expected Bencher metrics")?
            .iter()
            .map(|(name, values)| {
                let samples = match values["latency"].get("samples") {
                    Some(samples) => serde_json::from_value(samples.clone())?,
                    None => values["latency"]["value"].as_f64().into_iter().collect(),
                };
                let metrics = values
                    .as_object()
                    .context("expected benchmark metrics")?
                    .iter()
                    .filter(|(name, _)| *name != "latency")
                    .filter_map(|(name, metric)| {
                        metric["value"].as_f64().map(|v| (name.clone(), v))
                    })
                    .collect();
                Ok(Row {
                    name: name.clone(),
                    samples,
                    metrics,
                })
            })
            .collect();
    }
    let sample: Sample = serde_json::from_value(json)?;
    ensure!(
        !sample.times.is_empty() && sample.times.len() == sample.iters.len(),
        "invalid Criterion sample"
    );
    ensure!(
        sample.iters.iter().all(|n| n.is_finite() && *n > 0.0),
        "invalid Criterion iteration count"
    );
    let samples = sample
        .times
        .iter()
        .zip(&sample.iters)
        .map(|(time, iters)| time / iters)
        .collect();
    let benchmark = read_json(&path.with_file_name("benchmark.json"))?;
    let mut row = Row {
        name: benchmark["full_id"]
            .as_str()
            .context("missing Criterion benchmark name")?
            .into(),
        samples,
        metrics: BTreeMap::new(),
    };
    let mut sorted = row.samples.clone();
    sorted.sort_by(f64::total_cmp);
    let median = (sorted[(sorted.len() - 1) / 2] + sorted[sorted.len() / 2]) / 2.0;
    let throughput = &benchmark["throughput"];
    for (kind, unit) in [
        ("Bytes", "bytes/s"),
        ("BytesDecimal", "bytes/s"),
        ("Elements", "elements/s"),
        ("Bits", "bits/s"),
    ] {
        if let Some(amount) = throughput[kind].as_f64() {
            row.metrics.insert(unit.into(), amount * 1e9 / median);
        }
    }
    for (kind, unit) in [("elements", "elements/s"), ("bytes", "bytes/s")] {
        if let Some(amount) = throughput["ElementsAndBytes"][kind].as_f64() {
            row.metrics.insert(unit.into(), amount * 1e9 / median);
        }
    }
    Ok(vec![row])
}

pub(super) fn write(
    root: &Path,
    previous: &Snapshot,
    csv: &Path,
    output: &mut impl Write,
) -> Result<()> {
    let mut rows = Vec::new();
    for (path, metadata) in snapshot(root)? {
        if previous.get(&path) == Some(&metadata) {
            continue;
        }
        for row in read_rows(&path).with_context(|| format!("loading {}", path.display()))? {
            ensure!(
                row.samples.iter().all(|n| n.is_finite() && *n > 0.0),
                "invalid timings for {}",
                row.name
            );
            rows.push(row);
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let metrics: BTreeSet<String> = rows
        .iter()
        .flat_map(|r| r.metrics.keys().cloned())
        .collect();
    let mut header: Vec<String> = [
        "benchmark",
        "samples",
        "min_ns",
        "max_ns",
        "median_ns",
        "mean_ns",
        "std_dev_ns",
        "iterations/s",
    ]
    .map(str::to_owned)
    .into();
    header.extend(metrics.iter().cloned());
    let mut cells = vec![header];
    cells.extend(rows.into_iter().map(|row| row.cells(&metrics)));
    let mut file = fs::File::create(csv)?;
    for row in &cells {
        let escaped: Vec<String> = row
            .iter()
            .map(|cell| format!("\"{}\"", cell.replace('"', "\"\"")))
            .collect();
        writeln!(file, "{}", escaped.join(","))?;
    }
    if cells.len() == 1 {
        writeln!(output, "No new benchmark measurements.")?;
        return Ok(());
    }
    for row in cells.iter_mut().skip(1) {
        for cell in row.iter_mut().skip(2) {
            if let Ok(value) = cell.parse::<f64>() {
                *cell = format!("{value:.3}");
            }
        }
    }
    let widths: Vec<usize> = (0..cells[0].len())
        .map(|col| cells.iter().map(|r| r[col].len()).max().unwrap())
        .collect();
    writeln!(output)?;
    for row in &cells {
        for (cell, width) in row.iter().zip(&widths) {
            write!(output, "{cell:width$}  ")?;
        }
        writeln!(output)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path, name: &str, file: &str, value: Value) {
        let directory = root.join(name).join("new");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(file), serde_json::to_vec(&value).unwrap()).unwrap();
    }

    #[test]
    fn summarizes_current_criterion_and_bencher_measurements_as_table_and_csv() {
        let temp = tir_adt::TempDir::with_prefix("bench-report-").unwrap();
        let root = temp.path();
        fixture(
            root,
            "stale",
            "bencher.json",
            serde_json::json!({
                "old": {"latency": {"samples": [1000.0], "value": 1000.0}}
            }),
        );
        let before = snapshot(root).unwrap();
        fixture(
            root,
            "criterion",
            "sample.json",
            serde_json::json!({
                "iters": [1.0, 2.0, 3.0, 4.0], "times": [10.0, 40.0, 90.0, 160.0]
            }),
        );
        fixture(
            root,
            "criterion",
            "benchmark.json",
            serde_json::json!({
                "full_id": "lexer", "throughput": {"ElementsAndBytes": {"elements": 2, "bytes": 100}}
            }),
        );
        fixture(
            root,
            "experiment",
            "bencher.json",
            serde_json::json!({
                "compile,\"C\"": {"latency": {"samples": [30.0, 10.0, 20.0], "value": 20.0}, "memory": {"value": 4096}}
            }),
        );
        fixture(
            root,
            "metrics-only",
            "bencher.json",
            serde_json::json!({
                "memory-only": {"memory": {"value": 8192}}
            }),
        );
        let csv = root.join("benchmarks.csv");
        let mut table = Vec::new();
        write(root, &before, &csv, &mut table).unwrap();
        let csv = fs::read_to_string(csv).unwrap();
        let lines: Vec<_> = csv.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "\"benchmark\",\"samples\",\"min_ns\",\"max_ns\",\"median_ns\",\"mean_ns\",\"std_dev_ns\",\"iterations/s\",\"bytes/s\",\"elements/s\",\"memory\"");
        assert_eq!(lines[1], "\"compile,\"\"C\"\"\",\"3\",\"10\",\"30\",\"20\",\"20\",\"10\",\"50000000\",\"\",\"\",\"4096\"");
        assert!(lines[2].starts_with("\"lexer\",\"4\",\"10\",\"40\",\"25\",\"25\",\"12.909944487358056\",\"40000000\",\"4000000000\",\"80000000\",\"\""));
        assert_eq!(
            lines[3],
            "\"memory-only\",\"0\",\"\",\"\",\"\",\"\",\"\",\"\",\"\",\"\",\"8192\""
        );
        let table = String::from_utf8(table).unwrap();
        assert!(table.contains("compile,\"C\""));
        assert!(table.contains("12.910"));
        assert!(table.contains("bytes/s"));
        assert!(!table.contains("old"));
    }

    #[test]
    fn empty_runs_replace_csv_and_invalid_samples_report_an_error() {
        let temp = tir_adt::TempDir::with_prefix("bench-report-").unwrap();
        let root = temp.path();
        let csv = root.join("benchmarks.csv");
        fs::write(&csv, "obsolete measurements").unwrap();
        let mut table = Vec::new();
        write(root, &snapshot(root).unwrap(), &csv, &mut table).unwrap();
        assert_eq!(fs::read_to_string(&csv).unwrap().lines().count(), 1);
        assert_eq!(
            String::from_utf8(table).unwrap(),
            "No new benchmark measurements.\n"
        );
        fixture(
            root,
            "bad",
            "sample.json",
            serde_json::json!({"iters": [0.0], "times": [1.0]}),
        );
        let error = write(root, &Snapshot::new(), &csv, &mut Vec::new()).unwrap_err();
        assert!(format!("{error:#}").contains("invalid Criterion iteration count"));
    }
}
