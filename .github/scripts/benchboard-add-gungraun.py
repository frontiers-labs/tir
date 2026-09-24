#!/usr/bin/env python3
"""Add Gungraun Cachegrind summaries to a Benchboard run imported from TIR."""

import importlib.util
import json
import math
import sys
from pathlib import Path


# The importer and runner validate the same executable-to-case manifest.
_spec = importlib.util.spec_from_file_location(
    "nightly_benchmarks", Path(__file__).with_name("discover-nightly-benchmarks.py"))
_discovery = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_discovery)

METRICS = {"Ir", "Dr", "Dw", "I1mr", "D1mr", "D1mw", "ILmr", "DLmr", "DLmw", "Bc", "Bcm"}


def new_metric(entry):
    sides = entry["metrics"]
    if "Left" in sides:
        value = sides["Left"]
    elif "Both" in sides:
        value = sides["Both"][0]
    else:
        raise ValueError("Gungraun summary has no new metric")
    if not isinstance(value, dict) or len(value) != 1:
        raise ValueError("Gungraun summary has no new metric")
    number = value.get("Int", value.get("Float"))
    if type(number) not in (int, float) or not math.isfinite(number) or number < 0:
        raise ValueError("Gungraun summary has an invalid metric")
    return number


def add_summaries(run, directory):
    manifest = json.loads((directory / "manifest.json").read_text())
    if manifest.get("version") != 1:
        raise ValueError("Unsupported benchmark manifest")
    contract = manifest["contract"]
    if not isinstance(contract, str) or len(contract) != 12 or any(c not in "0123456789abcdef" for c in contract):
        raise ValueError("Invalid benchmark contract")
    cases = _discovery.expected_cases(manifest["targets"])
    files = sorted(directory.rglob("summary.json"))
    if not files:
        raise ValueError("Gungraun produced no summaries")
    seen = set()
    for file in files:
        summary = json.loads(file.read_text())
        if summary.get("version") != "6" or summary.get("kind") != "LibraryBenchmark":
            raise ValueError(f"{file}: unsupported Gungraun summary")
        case = (
            summary["benchmark_exe"],
            summary.get("function_name"),
            summary.get("id"),
        )
        if case not in cases or case in seen:
            raise ValueError(f"{file}: unexpected or duplicate function case {case}")
        seen.add(case)
        compiler = cases[case]["compiler"]
        profiles = [profile for profile in summary["profiles"] if profile["tool"] == "Cachegrind"]
        if len(profiles) != 1:
            raise ValueError(f"{file}: expected one Cachegrind profile")
        metrics = profiles[0]["summaries"]["total"]["summary"]["Cachegrind"]
        if "Ir" not in metrics or new_metric(metrics["Ir"]) == 0:
            raise ValueError(f"{file}: missing instruction count")
        for metric, entry in metrics.items():
            if metric not in METRICS:
                continue
            run["measurements"].append({
                "suite": "criterion",
                "benchmark": cases[case]["benchmark"],
                "compiler": compiler,
                "metric": metric,
                "unit": "count",
                "value": new_metric(entry),
                "gate": metric == "Ir" and compiler != "egg",
                "metadata": {"source": "gungraun", "engine": "cachegrind", "summary": str(file.relative_to(directory))},
            })
    if seen != set(cases):
        raise ValueError(f"Gungraun cases missing: {set(cases) - seen}")
    run["config"] += f";micro-contract={contract}"
    return run


def main():
    run_file, summary_dir = map(Path, sys.argv[1:])
    run = add_summaries(json.loads(run_file.read_text()), summary_dir)
    run_file.write_text(json.dumps(run, indent=2) + "\n")


if __name__ == "__main__":
    main()
