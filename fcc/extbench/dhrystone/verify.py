#!/usr/bin/env python3
"""Compare a Dhrystone executable with the recorded final values."""

from __future__ import annotations

import difflib
import re
import subprocess
import sys
from pathlib import Path


def normalize(output: str) -> str:
    output = output.split(
        "Final values of the variables used in the benchmark:", 1
    )[1]
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
    executable = Path(sys.argv[1]).resolve()
    args = sys.argv[2:] or ["100000000"]
    result = subprocess.run(
        [str(executable), *args], capture_output=True, text=True, check=True
    )
    expected = Path(__file__).with_name("expected.out").read_text()
    actual = normalize(result.stdout)
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
