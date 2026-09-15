#!/usr/bin/env python3
"""Validate CoreMark output without rerunning a saved benchmark result."""

from __future__ import annotations

import argparse
import re
import subprocess
from pathlib import Path


EXPECTED_CRC = {
    "seedcrc": "e9f5",
    "crclist": "e714",
    "crcmatrix": "1fd7",
    "crcstate": "8e3a",
}
EXPECTED_FINAL_CRC = {(0, 0, 0, 1_000_000): "988c"}
SHORT_RUN_ERROR = "ERROR! Must execute for at least 10 secs for a valid result!"


def validate(output: str, args: list[str]) -> None:
    if len(args) < 4:
        raise AssertionError("CoreMark requires seed arguments and an iteration count")
    try:
        seeds = tuple(int(value, 10) for value in args[:3])
        iterations = int(args[3], 10)
    except ValueError as error:
        raise AssertionError("invalid CoreMark seed or iteration argument") from error
    if iterations <= 0:
        raise AssertionError(f"invalid CoreMark iteration count: {iterations}")
    iteration_match = re.search(r"(?mi)^\s*Iterations\s*:\s*(\d+)\s*$", output)
    if iteration_match is None or int(iteration_match.group(1)) != iterations:
        observed = iteration_match.group(1) if iteration_match else "missing"
        raise AssertionError(f"CoreMark iterations differ: expected {iterations}, observed {observed}")
    time_match = re.search(r"(?mi)^\s*Total time \(secs\)\s*:\s*([0-9]+(?:\.[0-9]+)?)\s*$", output)
    if time_match is None:
        raise AssertionError("CoreMark total time is missing")
    short_run = float(time_match.group(1)) < 10
    short_run_error = any(line.strip() == SHORT_RUN_ERROR for line in output.splitlines())
    for line in output.splitlines():
        if re.search(r"(?i)error|failed|failure|cannot validate operation|errors detected", line):
            if line.strip() == SHORT_RUN_ERROR:
                continue
            if line.strip() == "Errors detected" and short_run and short_run_error:
                continue
            raise AssertionError("CoreMark output reports a runtime or correctness error")
    for name, expected in EXPECTED_CRC.items():
        match = re.search(rf"(?mi)^\s*(?:\[\d+\])?{name}\s*:\s*0x([0-9a-f]+)\s*$", output)
        if match is None or match.group(1).lower() != expected:
            observed = match.group(1) if match else "missing"
            raise AssertionError(f"CoreMark {name} differs: expected 0x{expected}, observed {observed}")
    final_match = re.search(r"(?mi)^\s*(?:\[\d+\])?crcfinal\s*:\s*0x([0-9a-f]+)\s*$", output)
    expected_final = EXPECTED_FINAL_CRC.get((*seeds, iterations))
    if final_match is None or expected_final is None or final_match.group(1).lower() != expected_final:
        observed = final_match.group(1) if final_match else "missing"
        raise AssertionError(f"CoreMark final CRC differs: expected 0x{expected_final or 'known value'}, observed {observed}")
    if not short_run:
        if "Correct operation validated." not in output:
            raise AssertionError("CoreMark did not report correct operation")
        if re.search(r"(?m)^\s*CoreMark\s+1\.0\s*:", output) is None:
            raise AssertionError("CoreMark output has no score")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--stdout", type=Path)
    parser.add_argument("values", nargs="*")
    parsed = parser.parse_args()
    if parsed.stdout is None:
        if not parsed.values:
            parser.error("an executable or --stdout is required")
        executable = Path(parsed.values[0]).resolve()
        args = parsed.values[1:] or ["0", "0", "0", "1000000"]
        result = subprocess.run([str(executable), *args], capture_output=True, text=True, check=False)
        if result.returncode != 0:
            raise RuntimeError(f"{executable} failed with status {result.returncode}")
        output = result.stdout
    else:
        output = parsed.stdout.read_text()
        args = parsed.values or ["0", "0", "0", "1000000"]
    validate(output, args)
    print("coremark: output is valid")


if __name__ == "__main__":
    main()
