#!/usr/bin/env python3
"""Compare a Dhrystone executable with the recorded final values."""

from __future__ import annotations

import argparse
import difflib
import re
import subprocess
from pathlib import Path


def normalize(output: str) -> str:
    marker = "Final values of the variables used in the benchmark:"
    if marker not in output:
        raise AssertionError("Dhrystone output has no final values")
    if re.search(r"(?i)\b(error|failed|failure|runtime|insufficient duration)\b", output):
        raise AssertionError("Dhrystone output reports a runtime error")
    output = output.split(marker, 1)[1]
    output = output.split("Measured time", 1)[0].split("Microseconds", 1)[0]
    pointers = re.findall(r"Ptr_Comp:\s+(\S+)", output)
    if (
        len(pointers) != 2
        or pointers[0] != pointers[1]
        or pointers[0] in {"(nil)", "0x0"}
        or int(pointers[0], 16) == 0
    ):
        raise AssertionError(f"incorrect record pointers: {pointers}")
    output = re.sub(r"(Ptr_Comp:\s+)\S+", r"\1<pointer>", output)
    return "\n".join(line.rstrip() for line in output.strip().splitlines()) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--stdout", type=Path)
    parser.add_argument("values", nargs="*")
    parsed = parser.parse_args()
    if parsed.stdout is None:
        if not parsed.values:
            parser.error("an executable or --stdout is required")
        executable = Path(parsed.values[0]).resolve()
        args = parsed.values[1:] or ["100000000"]
        result = subprocess.run(
            [str(executable), *args], capture_output=True, text=True, check=True
        )
        output = result.stdout
    else:
        output = parsed.stdout.read_text()
        args = parsed.values or ["100000000"]
    try:
        runs = int(args[0])
    except (IndexError, ValueError) as error:
        raise AssertionError(f"invalid Dhrystone run count: {args}") from error
    if runs <= 0:
        raise AssertionError(f"invalid Dhrystone run count: {runs}")
    start = re.search(r"Execution starts,\s+(\d+) runs through Dhrystone", output)
    if start is None or int(start.group(1)) != runs:
        observed = start.group(1) if start else "missing"
        raise AssertionError(f"Dhrystone runs differ: expected {runs}, observed {observed}")
    expected = Path(__file__).with_name("expected.out").read_text()
    expected = expected.replace("100000010", str(runs + 10))
    actual = normalize(output)
    if actual != expected:
        raise AssertionError(
            "".join(
                difflib.unified_diff(
                    expected.splitlines(keepends=True),
                    actual.splitlines(keepends=True),
                    fromfile="expected.out",
                    tofile="actual",
                )
            )
        )
    print("dhrystone: output matches")


if __name__ == "__main__":
    main()
