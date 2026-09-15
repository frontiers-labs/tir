use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn command(mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
    command.args(["extbench", mode, "--suite"]);
    command.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("checks/Inputs/local/bench_suite.toml"));
    command
}

fn suite_command(mode: &str, suite: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
    command.args(["extbench", mode, "--suite"]);
    command.arg(suite);
    command
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn results(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn even_repeated_runs_report_median_and_peak_with_raw_measurements() {
    let temporary = tempfile::tempdir().unwrap();
    let output = temporary.path().join("results.json");
    success(
        command("run")
            .args(["--runs", "2", "--level=-O0", "--output"])
            .arg(&output)
            .output()
            .unwrap(),
    );
    let results = results(&output);
    let sample = &results["samples"][0];
    let runs = sample["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    let mean = runs
        .iter()
        .map(|run| run["wall_ms"].as_f64().unwrap())
        .sum::<f64>()
        / 2.0;
    assert_eq!(sample["wall_ms"].as_f64().unwrap(), mean);
    let maximum = runs
        .iter()
        .map(|run| run["peak_rss_kb"].as_u64().unwrap())
        .max()
        .unwrap();
    assert_eq!(sample["peak_rss_kb"].as_u64().unwrap(), maximum);
}

#[test]
fn odd_repeated_runs_report_median() {
    let temporary = tempfile::tempdir().unwrap();
    let output = temporary.path().join("results.json");
    success(
        command("run")
            .args(["--runs", "3", "--level=-O0", "--output"])
            .arg(&output)
            .output()
            .unwrap(),
    );
    let results = results(&output);
    let sample = &results["samples"][0];
    let runs = sample["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3);
    let mut wall_times = runs
        .iter()
        .map(|run| run["wall_ms"].as_f64().unwrap())
        .collect::<Vec<_>>();
    wall_times.sort_by(f64::total_cmp);
    assert_eq!(sample["wall_ms"].as_f64().unwrap(), wall_times[1]);
}

#[test]
fn opt_in_compiler_requires_an_explicit_filter() {
    let temporary = tempfile::tempdir().unwrap();
    let default_output = temporary.path().join("default.json");
    let explicit_output = temporary.path().join("explicit.json");
    success(
        command("compile")
            .args(["--input", "llvm", "--output"])
            .arg(&default_output)
            .output()
            .unwrap(),
    );
    success(
        command("compile")
            .args(["--input", "llvm", "--compiler", "tir-opt-in", "--output"])
            .arg(&explicit_output)
            .output()
            .unwrap(),
    );
    let default_results = results(&default_output);
    let default_compilers = default_results["samples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|sample| sample["compiler"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(default_compilers, ["tir2", "tir"]);
    assert_eq!(
        results(&explicit_output)["samples"][0]["compiler"],
        "tir-opt-in"
    );
}

#[test]
fn workload_digest_frames_source_file_boundaries() {
    let temporary = tempfile::tempdir().unwrap();
    let suite = temporary.path().join("bench_suite.toml");
    let benchmark = temporary.path().join("collision");
    std::fs::create_dir(&benchmark).unwrap();
    std::fs::write(
        &suite,
        r#"
[suite]
package = "fixture"

[[compiler]]
name = "copy"
compile = ["cp", "{source}", "{output}"]
link = []
"#,
    )
    .unwrap();
    std::fs::write(
        benchmark.join("benchmark.toml"),
        "sources = [\"a\", \"b\"]\nlevels = [\"-O0\"]\n",
    )
    .unwrap();
    std::fs::write(benchmark.join("a"), "ab").unwrap();
    std::fs::write(benchmark.join("b"), "c").unwrap();
    let baseline = temporary.path().join("baseline.json");
    success(
        suite_command("compile", &suite)
            .args(["--output"])
            .arg(&baseline)
            .output()
            .unwrap(),
    );
    std::fs::write(benchmark.join("a"), "a").unwrap();
    std::fs::write(benchmark.join("b"), "bc").unwrap();
    let output = suite_command("compile", &suite)
        .arg("--baseline")
        .arg(&baseline)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("baseline workload differs"));
}

#[test]
fn artifacts_retain_build_products_and_each_run_output() {
    let temporary = tempfile::tempdir().unwrap();
    let artifacts = temporary.path().join("artifacts");
    success(
        command("run")
            .args(["--runs", "2", "--artifacts"])
            .arg(&artifacts)
            .output()
            .unwrap(),
    );
    let compiler = artifacts.join("fixture/integer/O0/gcc");
    for file in [
        "0.o",
        "benchmark-0",
        "run-0/run-0.stdout",
        "run-0/run-0.stderr",
        "run-0/run-1.stdout",
        "run-0/run-1.stderr",
    ] {
        assert!(compiler.join(file).is_file(), "missing {file}");
    }
}

#[test]
fn existing_artifact_directory_is_not_overwritten() {
    let temporary = tempfile::tempdir().unwrap();
    let marker = temporary.path().join("marker");
    std::fs::write(&marker, "keep").unwrap();
    let output = command("run")
        .arg("--artifacts")
        .arg(temporary.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("artifact directory already exists"));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "keep");
}

#[test]
fn compiler_filter_preserves_prepared_input_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let all = temporary.path().join("all.json");
    let one = temporary.path().join("one.json");
    success(
        command("compile")
            .args(["--input", "llvm", "--output"])
            .arg(&all)
            .output()
            .unwrap(),
    );
    success(
        command("compile")
            .args(["--input", "llvm", "--compiler", "tir", "--output"])
            .arg(&one)
            .output()
            .unwrap(),
    );
    assert_eq!(results(&all)["producer"], results(&one)["producer"]);
}

#[test]
fn compatible_baseline_is_compared_and_regressions_are_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let baseline_path = temporary.path().join("baseline.json");
    success(
        command("compile")
            .args(["--input", "llvm", "--compiler", "tir", "--output"])
            .arg(&baseline_path)
            .output()
            .unwrap(),
    );
    let mut baseline = results(&baseline_path);
    baseline["samples"][0]["wall_ms"] = Value::from(1_000_000.0);
    baseline["samples"][0]["peak_rss_kb"] = Value::from(1_000_000);
    std::fs::write(&baseline_path, serde_json::to_vec(&baseline).unwrap()).unwrap();
    let output = success(
        command("compile")
            .args(["--input", "llvm", "--compiler", "tir", "--baseline"])
            .arg(&baseline_path)
            .output()
            .unwrap(),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("tir -O0 baseline"));
    baseline["samples"][0]["wall_ms"] = Value::from(0.0);
    std::fs::write(&baseline_path, serde_json::to_vec(&baseline).unwrap()).unwrap();
    let output = command("compile")
        .args(["--input", "llvm", "--compiler", "tir", "--baseline"])
        .arg(&baseline_path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("time grew by more than 10%"));
}
