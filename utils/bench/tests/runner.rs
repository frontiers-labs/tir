#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Value, json};
use tir_bench::{BatchSize, Criterion, Options, ProcessCase, Variant, process::Command};

const NAMESPACE: &str = "tir-bench/runner-test";

fn options(output: &Path) -> Options {
    let mut options =
        Options::try_parse_from(["runner", "--sample-size", "2", "--warmups", "0"]).unwrap();
    options.output = Some(output.to_owned());
    options
}

fn process_case(script: &str, metadata: Value) -> ProcessCase {
    ProcessCase {
        id: format!("{NAMESPACE}/process"),
        command: Command::new("/bin/sh").args(["-c", script]),
        verify: Some(Box::new(|stdout| {
            let text = std::fs::read_to_string(stdout)?;
            anyhow::ensure!(text == "valid\n", "unexpected output: {text:?}");
            Ok(())
        })),
        metadata,
        gate: true,
        variant: Some(Variant {
            benchmark: "process".into(),
            group: "Run".into(),
            variant: "candidate".into(),
            subject: true,
        }),
    }
}

fn run_suite(
    options: Options,
    script: &str,
    metadata: Value,
    include_case: bool,
) -> (PathBuf, tir_bench::Result<()>) {
    let mut criterion = Criterion::new(NAMESPACE, options).unwrap();
    let path = criterion.artifacts().unwrap();
    if include_case {
        criterion.bench_processes(vec![process_case(script, metadata)]);
    }
    (path, criterion.finish())
}

fn result(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path.join("results.json")).unwrap()).unwrap()
}

// Benchboard reads this file, so its shape is a contract with another repository.
fn summary_exports_samples_and_shape(path: &Path, latencies: &[f64]) {
    let summary: Value =
        serde_json::from_slice(&std::fs::read(path.join("summary.json")).unwrap()).unwrap();
    let architecture = std::env::consts::ARCH;
    let id = format!("{architecture}/{NAMESPACE}/process");
    let latency = &summary["results"][&id]["latency"];
    let spread = latencies[1] - latencies[0];
    assert_eq!(latency["value"], (latencies[0] + latencies[1]) / 2.0);
    assert_eq!(latency["lower_value"], latencies[0] + spread / 4.0);
    assert_eq!(latency["upper_value"], latencies[0] + spread * 3.0 / 4.0);
    let mut exported_samples: Vec<f64> =
        serde_json::from_value(latency["samples"].clone()).unwrap();
    exported_samples.sort_by(f64::total_cmp);
    assert_eq!(exported_samples, latencies);
    assert!(latency["modality"].is_string());
    assert!(
        summary["environment"]
            .as_str()
            .unwrap()
            .starts_with(architecture)
    );
    // CPU times and the deviation are sampled, and only defined metrics are exported.
    let exported: Vec<_> = summary["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|metric| {
            (
                metric["key"].as_str().unwrap(),
                metric["unit"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        exported,
        [("latency", "ns"), ("peak_process_rss_bytes", "bytes")]
    );
    assert_eq!(
        summary["variants"][&id],
        json!({"benchmark": "process", "group": format!("Run on {architecture}"), "variant": "candidate", "subject": true})
    );
}

#[test]
fn native_suite_records_validates_and_compares() {
    let temp = tir_adt::TempDir::new().unwrap();
    let workload = json!({"workload": "fixed-output-v1"});

    let (baseline_path, baseline_status) = run_suite(
        options(temp.path()),
        "printf 'valid\\n'",
        workload.clone(),
        true,
    );
    baseline_status.unwrap();
    let baseline = result(&baseline_path);
    assert_eq!(baseline["status"], "complete");
    assert_eq!(baseline["cases"].as_array().unwrap().len(), 1);
    let process = baseline["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == format!("{NAMESPACE}/process"))
        .unwrap();
    let samples = process["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 2);
    for key in [
        "latency",
        "user_cpu_ns",
        "system_cpu_ns",
        "peak_process_rss_bytes",
    ] {
        assert!(samples.iter().all(|sample| sample[key].as_f64().is_some()));
    }
    let mut latencies = samples
        .iter()
        .map(|s| s["latency"].as_f64().unwrap())
        .collect::<Vec<_>>();
    latencies.sort_by(f64::total_cmp);
    let median = (latencies[0] + latencies[1]) / 2.0;
    assert_eq!(process["summary"]["latency"].as_f64().unwrap(), median);
    summary_exports_samples_and_shape(&baseline_path, &latencies);

    let mut compatible = options(temp.path());
    compatible.baseline = Some(baseline_path.clone());
    compatible.threshold = 1_000_000.0;
    compatible.rss_threshold = 1_000_000.0;
    let (compatible_path, status) =
        run_suite(compatible, "printf 'valid\\n'", workload.clone(), true);
    status.unwrap();
    assert_ne!(compatible_path, baseline_path);
    assert_eq!(result(&baseline_path), baseline);

    let mut changed_workload = options(temp.path());
    changed_workload.baseline = Some(baseline_path.clone());
    let (path, status) = run_suite(
        changed_workload,
        "printf 'valid\\n'",
        json!({"workload": "v2"}),
        true,
    );
    assert!(
        status
            .unwrap_err()
            .to_string()
            .contains("baseline workload differs")
    );
    assert_eq!(result(&path)["status"], "incomplete");

    let mut missing_case = options(temp.path());
    missing_case.baseline = Some(baseline_path.clone());
    let (path, status) = run_suite(missing_case, "printf 'valid\\n'", workload.clone(), false);
    assert!(
        status
            .unwrap_err()
            .to_string()
            .contains("baseline case inventory differs")
    );
    assert_eq!(result(&path)["status"], "incomplete");

    let mut slower = options(temp.path());
    slower.baseline = Some(baseline_path);
    slower.threshold = 0.0;
    slower.rss_threshold = 1_000_000.0;
    let (path, status) = run_suite(slower, "sleep 0.2; printf 'valid\\n'", workload, true);
    assert!(
        status
            .unwrap_err()
            .to_string()
            .contains("benchmark regressions")
    );
    assert_eq!(result(&path)["status"], "incomplete");

    let (invalid_path, status) = run_suite(
        options(temp.path()),
        "printf 'wrong\\n'",
        json!({"workload": "invalid"}),
        true,
    );
    assert!(status.unwrap_err().to_string().contains("validator"));
    let invalid_result = result(&invalid_path);
    assert_eq!(invalid_result["status"], "incomplete");
    assert!(invalid_result["cases"].as_array().unwrap().is_empty());

    // A failed group stops the benchmarks after it. The host lock allows one
    // active harness, so this shares the test above.
    let marker = temp.path().join("ran");
    let mut criterion = Criterion::new(NAMESPACE, options(temp.path())).unwrap();
    criterion.bench_processes(vec![process_case("printf 'wrong\\n'", json!({}))]);
    criterion.bench_processes(vec![ProcessCase {
        id: format!("{NAMESPACE}/later"),
        command: Command::new("/bin/sh").args(["-c", &format!("touch {}", marker.display())]),
        verify: None,
        metadata: json!({}),
        gate: true,
        variant: None,
    }]);
    assert!(criterion.finish().is_err());
    assert!(!marker.exists());

    timed_functions_are_recorded_with_their_samples(&temp);
}

// The host lock allows one active harness, so the suite test calls this.
fn timed_functions_are_recorded_with_their_samples(temp: &tir_adt::TempDir) {
    let mut options = Options::try_parse_from([
        "runner",
        "--sample-size",
        "10",
        "--warm-up-time",
        "0.01",
        "--measurement-time",
        "0.05",
    ])
    .unwrap();
    options.output = Some(temp.path().to_owned());
    let mut criterion = Criterion::new(NAMESPACE, options).unwrap();
    let mut group = criterion.benchmark_group("group");
    group.bench_function("batched", |b| {
        b.iter_batched(|| 7, |input| input * 2, BatchSize::SmallInput);
    });
    group.finish();
    let path = criterion.artifacts().unwrap();
    criterion.finish().unwrap();
    let results = result(&path);
    let case = &results["cases"][0];
    assert_eq!(case["id"], "group/batched");
    assert_eq!(case["samples"].as_array().unwrap().len(), 10);
}
