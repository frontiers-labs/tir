#!/usr/bin/env python3
"""Discover, describe, and profile the workspace's function benchmarks."""

import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
FEATURES = ["--features", "nightly-cachegrind"]


def bench_targets(metadata):
    members = set(metadata["workspace_members"])
    features = {node["id"]: set(node["features"]) for node in metadata["resolve"]["nodes"]}
    return {
        (package["id"], target["name"]): (package, target)
        for package in metadata["packages"] if package["id"] in members
        for target in package["targets"] if "bench" in target["kind"]
        if set(target.get("required-features", [])) <= features[package["id"]]
    }


def bench_artifacts(messages, expected):
    artifacts = {}
    for message in messages:
        if message.get("reason") != "compiler-artifact" or "bench" not in message["target"]["kind"]:
            continue
        key = (message["package_id"], message["target"]["name"])
        if key not in expected or key in artifacts or not message.get("executable"):
            raise ValueError(f"Unexpected, duplicate, or missing benchmark executable: {key}")
        artifacts[key] = message["executable"]
    if artifacts.keys() != expected.keys():
        raise ValueError(f"Missing benchmark targets: {sorted(expected.keys() - artifacts.keys())}")
    return artifacts


def describe_targets(expected, artifacts):
    targets = []
    for key in sorted(expected):
        package, target = expected[key]
        package_dir = str(Path(package["manifest_path"]).parent)
        result = subprocess.check_output([artifacts[key], "--describe"], cwd=package_dir, text=True)
        description = json.loads(result)
        if description.get("kind") not in ("function", "program"):
            raise ValueError(f"{key}: missing benchmark kind")
        cases = description.get("cases", [])
        if description["kind"] == "function" and not cases:
            raise ValueError(f"{key}: no function cases")
        if description["kind"] == "program" and cases:
            raise ValueError(f"{key}: program declares function cases")
        inputs = description.get("inputs", [])
        if not isinstance(inputs, list) or not all(isinstance(value, str) for value in inputs):
            raise ValueError(f"{key}: invalid benchmark inputs")
        input_digest = hashlib.sha256(json.dumps(inputs).encode()).hexdigest()
        targets.append({
            "package_id": key[0],
            "target": key[1],
            "package_dir": package_dir,
            "benchmark_file": target["src_path"],
            "benchmark_exe": artifacts[key],
            "kind": description["kind"],
            "cases": cases,
            "input_digest": input_digest,
        })
    return targets


def expected_cases(targets):
    """Validate the manifest before profiling and again before importing it."""
    cases = {}
    measurements = set()
    executables = set()
    for target in targets:
        executable = target["benchmark_exe"]
        if not isinstance(executable, str) or not executable or executable in executables:
            raise ValueError("Invalid or duplicate benchmark executable")
        executables.add(executable)
        if target["kind"] == "program" and not target["cases"]:
            continue
        if target["kind"] != "function" or not target["cases"]:
            raise ValueError("Invalid benchmark kind or empty function benchmark")
        for case in target["cases"]:
            if (not isinstance(case["function_name"], str) or not case["function_name"]
                    or (case["id"] is not None and (not isinstance(case["id"], str) or not case["id"]))
                    or not isinstance(case["benchmark"], str) or not case["benchmark"]
                    or case["compiler"] not in ("tir", "fcc", "egg")):
                raise ValueError(f"Invalid function case: {case}")
            key = (executable, case["function_name"], case["id"])
            measurement = (case["benchmark"], case["compiler"])
            if key in cases or measurement in measurements:
                raise ValueError(f"Duplicate function case: {case}")
            cases[key] = case
            measurements.add(measurement)
    if not cases:
        raise ValueError("No function benchmarks discovered")
    return cases


def contract_hash(expected, targets):
    paths = {
        ROOT / "Cargo.toml", ROOT / "Cargo.lock", ROOT / "benchmarks/functions.rs",
        Path(__file__).resolve(), ROOT / ".github/scripts/benchboard-add-gungraun.py",
        ROOT / ".github/scripts/run-nightly-benchmarks.sh",
    }
    for package, target in expected.values():
        paths.add(Path(package["manifest_path"]))
        source = Path(target["src_path"])
        paths.add(source)
        # Helpers and input fixtures affect what each benchmark measures.
        paths.update(path for path in source.parent.rglob("*") if path.is_file())
    contract = hashlib.sha256()
    for path in sorted(paths):
        contract.update(str(path.relative_to(ROOT)).encode() + b"\0")
        contract.update(path.read_bytes() + b"\0")
    for target in targets:
        contract.update(target["input_digest"].encode())
    return contract.hexdigest()[:12]


def main():
    os.chdir(ROOT)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version=1", *FEATURES], text=True))
    expected = bench_targets(metadata)
    messages = []
    with subprocess.Popen(
        ["cargo", "bench", "--workspace", "--locked", "--no-run", "--message-format=json", *FEATURES],
        stdout=subprocess.PIPE, text=True,
    ) as build:
        for line in build.stdout:
            message = json.loads(line)
            if message.get("reason") == "compiler-message":
                print(message["message"].get("rendered", ""), end="", file=sys.stderr)
            elif message.get("reason") == "compiler-artifact":
                messages.append(message)
        if build.wait():
            raise RuntimeError("Benchmark build failed")
    targets = describe_targets(expected, bench_artifacts(messages, expected))
    expected_cases(targets)
    directory = Path(metadata["target_directory"]) / "gungraun"
    # Build and description must succeed before replacing previous measurements.
    if directory.exists():
        shutil.rmtree(directory)
    directory.mkdir(parents=True)
    manifest = {"version": 1, "contract": contract_hash(expected, targets), "targets": targets}
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for target in targets:
        if target["kind"] == "function":
            subprocess.run([target["benchmark_exe"], "--save-summary=json"],
                           cwd=target["package_dir"], check=True)


if __name__ == "__main__":
    main()
