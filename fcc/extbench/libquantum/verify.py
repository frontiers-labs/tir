#!/usr/bin/env python3
"""Check libquantum's deterministic factorization output."""

import argparse
import difflib
import subprocess
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--stdout", type=Path)
    parser.add_argument("values", nargs="*")
    parsed = parser.parse_args()
    args = (parsed.values[1:] if parsed.stdout is None else parsed.values) or [
        "143",
        "5",
    ]
    if args != ["143", "5"]:
        raise AssertionError(f"unverified libquantum workload: {args}")
    if parsed.stdout is None:
        if not parsed.values:
            parser.error("an executable or --stdout is required")
        executable = Path(parsed.values[0]).resolve()
        output = subprocess.run(
            [str(executable), *args], capture_output=True, text=True, check=True
        ).stdout
    else:
        output = parsed.stdout.read_text()
    expected = Path(__file__).with_name("expected.out").read_text()
    if output != expected:
        raise AssertionError(
            "".join(
                difflib.unified_diff(
                    expected.splitlines(keepends=True),
                    output.splitlines(keepends=True),
                    fromfile="expected.out",
                    tofile="actual",
                )
            )
        )
    print("libquantum: output matches")


if __name__ == "__main__":
    main()
