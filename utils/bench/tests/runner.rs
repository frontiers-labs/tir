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
    // Benchboard reads this file, so its shape is a contract with another repository.
    let summary: Value =
        serde_json::from_slice(&std::fs::read(baseline_path.join("summary.json")).unwrap())
            .unwrap();
    let deviation = (latencies[1] - latencies[0]) / 2.0;
    assert_eq!(
        summary["results"][format!("{NAMESPACE}/process")]["latency"],
        json!({"value": median, "lower_value": median - deviation, "upper_value": median + deviation})
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
        summary["variants"][format!("{NAMESPACE}/process")],
        json!({"benchmark": "process", "group": "Run", "variant": "candidate", "subject": true})
    );

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
}

/// The Cachegrind parent starts this binary once per function with
/// `--count-function`. The child must run that routine's counted region once.
#[test]
fn a_counting_child_runs_only_the_requested_function_once() {
    let child = |function: &str| {
        Criterion::new(
            NAMESPACE,
            Options::try_parse_from(["runner", "--count-function", function]).unwrap(),
        )
        .unwrap()
    };
    let calls = std::cell::RefCell::new(Vec::new());
    let call = |name| calls.borrow_mut().push(name);
    let mut criterion = child("group/batched");
    let mut group = criterion.benchmark_group("group");
    group.bench_function("plain", |b| b.iter(|| call("plain")));
    group.bench_function("batched", |b| {
        b.iter_batched(
            || call("setup"),
            |()| call("routine"),
            BatchSize::SmallInput,
        );
    });
    group.bench_with_input("input", &7, |b, _| b.iter(|| call("input")));
    group.finish();
    criterion.finish().unwrap();
    assert_eq!(*calls.borrow(), ["setup", "routine"]);

    let mut criterion = child("group/missing");
    criterion
        .benchmark_group("group")
        .bench_function("plain", |b| b.iter(|| ()));
    let error = criterion.finish().unwrap_err().to_string();
    assert!(error.contains("group/missing is not registered"), "{error}");
}
