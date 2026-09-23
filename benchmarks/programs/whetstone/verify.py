#!/usr/bin/env python3
"""Compare Whetstone's diagnostic module states across FCC, GCC, and Clang."""

from __future__ import annotations

import argparse
import math
import re
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path


MODULES = (1, 2, 3, 4, 6, 7, 8, 9, 10, 11)
COUNTS = (
    (0, 0, 0),
    (12, 14, 12),
    (14, 12, 12),
    (345, 0, 0),
    (210, 1, 2),
    (32, 1, 2),
    (899, 1, 2),
    (616, 1, 2),
    (0, 2, 3),
    (93, 2, 3),
)
RESULT = re.compile(
    r"^\s*(-?\d+)\s+(-?\d+)\s+(-?\d+)"
    r"\s+([^\s]+)\s+([^\s]+)\s+([^\s]+)\s+([^\s]+)\s*$"
)


@dataclass(frozen=True)
class ModuleResult:
    counts: tuple[int, int, int]
    values: tuple[float, float, float, float]
    spellings: tuple[str, str, str, str]


def parse_results(output: str) -> list[ModuleResult]:
    results: list[ModuleResult] = []
    for line in output.splitlines():
        match = RESULT.match(line)
        if match is None:
            continue
        counts = tuple(int(value) for value in match.groups()[:3])
        spellings = match.groups()[3:]
        values = tuple(float(value) for value in spellings)
        if not all(math.isfinite(value) for value in values):
            raise AssertionError(f"nonfinite Whetstone result: {line}")
        results.append(ModuleResult(counts, values, spellings))
    if len(results) != len(MODULES):
        raise AssertionError(f"expected {len(MODULES)} module results, found {len(results)}")
    actual_counts = tuple(result.counts for result in results)
    if actual_counts != COUNTS:
        raise AssertionError(f"missing or reordered module results: {actual_counts}")
    return results


def printed_quantum(spelling: str) -> float:
    _, exponent = spelling.lower().split("e")
    fraction_digits = len(spelling.lower().split("e")[0].split(".")[1])
    return 10.0 ** (int(exponent) - fraction_digits)


def compare_results(
    candidate_name: str,
    candidate: list[ModuleResult],
    reference_name: str,
    reference: list[ModuleResult],
) -> None:
    for module, actual, expected in zip(MODULES, candidate, reference, strict=True):
        if actual.counts != expected.counts:
            raise AssertionError(
                f"module {module} counts differ: {candidate_name}={actual.counts}, "
                f"{reference_name}={expected.counts}"
            )
        for index, (value, spelling, expected_value, expected_spelling) in enumerate(
            zip(actual.values, actual.spellings, expected.values, expected.spellings, strict=True),
            start=1,
        ):
            tolerance = max(printed_quantum(spelling), printed_quantum(expected_spelling))
            if abs(value - expected_value) > tolerance:
                raise AssertionError(
                    f"module {module} value {index} differs: "
                    f"{candidate_name}={spelling}, {reference_name}={expected_spelling}, "
                    f"tolerance={tolerance:.1e}"
                )


def validate_comparator(reference: list[ModuleResult]) -> None:
    first = reference[0]
    corrupted_values = list(first.values)
    corrupted_values[0] += 10.0 * printed_quantum(first.spellings[0])
    corrupted = list(reference)
    corrupted[0] = ModuleResult(first.counts, tuple(corrupted_values), first.spellings)
    try:
        compare_results("corrupted", corrupted, "reference", reference)
    except AssertionError:
        return
    raise AssertionError("the numerical comparator accepted a deliberately corrupted result")


def compile_and_run(
    compiler: str,
    is_fcc: bool,
    level: str,
    source: Path,
    directory: Path,
) -> list[ModuleResult]:
    executable = directory / f"{Path(compiler).name}-{level[1:]}"
    command = [compiler]
    if is_fcc:
        command.append("cc")
    command.extend(
        ["-std=gnu17", level, "-DPRINTOUT", str(source), "-lm", "-o", str(executable)]
    )
    compiled = subprocess.run(command, capture_output=True, text=True, check=False)
    if compiled.returncode != 0:
        raise RuntimeError(f"{' '.join(command)} failed:\n{compiled.stderr}")
    run = subprocess.run([str(executable), "1"], capture_output=True, text=True, check=False)
    if run.returncode not in (0, 1):
        raise RuntimeError(f"{executable} failed with status {run.returncode}:\n{run.stderr}")
    if run.returncode == 1 and "Insufficient duration" not in run.stdout:
        raise RuntimeError(f"{executable} failed before completing the modules:\n{run.stdout}")
    return parse_results(run.stdout)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fcc", default="fcc")
    parser.add_argument("--gcc", default="gcc")
    parser.add_argument("--clang", default="clang")
    args = parser.parse_args()
    source = Path(__file__).with_name("whetstone.c").resolve()
    with tempfile.TemporaryDirectory(prefix="whetstone-verify-") as temporary:
        directory = Path(temporary)
        for level in ("-O0", "-O2"):
            results = {
                "fcc": compile_and_run(args.fcc, True, level, source, directory),
                "gcc": compile_and_run(args.gcc, False, level, source, directory),
                "clang": compile_and_run(args.clang, False, level, source, directory),
            }
            validate_comparator(results["gcc"])
            compare_results("gcc", results["gcc"], "clang", results["clang"])
            compare_results("fcc", results["fcc"], "gcc", results["gcc"])
            compare_results("fcc", results["fcc"], "clang", results["clang"])
            print(f"{level}: all {len(MODULES)} module states match GCC and Clang")


if __name__ == "__main__":
    main()

