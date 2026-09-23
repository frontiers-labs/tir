#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Value, json};
use tir_bench::{Options, ProcessCase, Suite, process::Command};

const NAMESPACE: &str = "tir-bench/runner-test";

fn options(output: &Path) -> Options {
    let mut options = Options::try_parse_from([
        "runner",
        "--samples",
        "2",
        "--warmups",
        "0",
        "--iterations",
        "1",
    ])
    .unwrap();
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
    }
}

fn add_function(suite: &mut Suite) -> tir_bench::Result<()> {
    let setups = Cell::new(0);
    let operations = Cell::new(0);
    suite.function("batched", |b| {
        b.iter_batched(
            || {
                setups.set(setups.get() + 1);
                7
            },
            |input| {
                operations.set(operations.get() + 1);
                input + 1
            },
        );
    })?;
    assert_eq!(setups.get(), 2);
    assert_eq!(operations.get(), 2);
    Ok(())
}

fn run_suite(
    options: Options,
    script: &str,
    metadata: Value,
    include_function: bool,
) -> (PathBuf, tir_bench::Result<()>) {
    let mut suite = Suite::new(NAMESPACE, options).unwrap();
    let path = suite.artifacts().to_owned();
    suite
        .process_group(vec![process_case(script, metadata)])
        .unwrap();
    if include_function {
        add_function(&mut suite).unwrap();
    }
    (path, suite.finish())
}

fn result(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path.join("results.json")).unwrap()).unwrap()
}

#[test]
fn native_suite_records_validates_and_compares() {
    let temp = tempfile::tempdir().unwrap();
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
    assert_eq!(baseline["cases"].as_array().unwrap().len(), 2);
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
    let bmf: Value =
        serde_json::from_slice(&std::fs::read(baseline_path.join("summary.bmf.json")).unwrap())
            .unwrap();
    assert_eq!(
        bmf[format!("{NAMESPACE}/process")]["latency"]["value"],
        json!(median)
    );
    assert!(bmf[format!("{NAMESPACE}/process")]["peak_process_rss_bytes"]["value"].is_number());
    assert!(bmf[format!("{NAMESPACE}/batched")]["latency"]["value"].is_number());

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

    let mut invalid = Suite::new(NAMESPACE, options(temp.path())).unwrap();
    let invalid_path = invalid.artifacts().to_owned();
    let error = invalid
        .process_group(vec![process_case(
            "printf 'wrong\\n'",
            json!({"workload": "invalid"}),
        )])
        .unwrap_err();
    assert!(error.to_string().contains("validator"));
    let invalid_result = result(&invalid_path);
    assert_eq!(invalid_result["status"], "incomplete");
    assert!(invalid_result["cases"].as_array().unwrap().is_empty());
}

#[test]
fn relative_filters_survive_qualified_dispatch() {
    let options = Options::try_parse_from(["runner", "--list", "--filter", "parse"]).unwrap();
    let suite = Suite::new("fixture/parser", options).unwrap();
    assert!(suite.matches("parse"));
    assert!(suite.matches("fixture/parser/parse"));
    assert!(!suite.matches("fixture/parser/other"));
}
