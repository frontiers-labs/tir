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


def validate(output: str, args: list[str]) -> None:
    if len(args) < 4:
        raise AssertionError("CoreMark requires seed arguments and an iteration count")
    try:
        iterations = int(args[3], 10)
    except ValueError as error:
        raise AssertionError(f"invalid CoreMark iteration count: {args[3]!r}") from error
    if iterations <= 0:
        raise AssertionError(f"invalid CoreMark iteration count: {iterations}")
    if re.search(r"(?i)\b(error|failed|failure|runtime)\b", output):
        raise AssertionError("CoreMark output reports a runtime error")
    iteration_match = re.search(r"(?mi)^\s*Iterations\s*:\s*(\d+)\s*$", output)
    if iteration_match is None or int(iteration_match.group(1)) != iterations:
        observed = iteration_match.group(1) if iteration_match else "missing"
        raise AssertionError(f"CoreMark iterations differ: expected {iterations}, observed {observed}")
    time_match = re.search(r"(?mi)^\s*Total time \(secs\)\s*:\s*([0-9]+(?:\.[0-9]+)?)\s*$", output)
    if time_match is None or float(time_match.group(1)) < 10:
        observed = time_match.group(1) if time_match else "missing"
        raise AssertionError(f"CoreMark run is too short: expected at least 10 seconds, observed {observed}")
    for name, expected in EXPECTED_CRC.items():
        match = re.search(rf"(?mi)^\s*(?:\[\d+\])?{name}\s*:\s*0x([0-9a-f]+)\s*$", output)
        if match is None or match.group(1).lower() != expected:
            observed = match.group(1) if match else "missing"
            raise AssertionError(f"CoreMark {name} differs: expected 0x{expected}, observed {observed}")
    if re.search(r"(?mi)^\s*(?:\[\d+\])?crcfinal\s*:\s*0x[0-9a-f]+\s*$", output) is None:
        raise AssertionError("CoreMark output has no final CRC")
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
