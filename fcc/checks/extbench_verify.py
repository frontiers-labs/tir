#!/usr/bin/env python3
"""Exercise the external benchmark output validators with saved output."""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def run(validator: Path, output: str, args: list[str]) -> subprocess.CompletedProcess[str]:
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as fixture:
        fixture.write(output)
        fixture.flush()
        return subprocess.run(
            [sys.executable, str(validator), "--stdout", fixture.name, *args],
            capture_output=True,
            text=True,
            check=False,
        )


def test_coremark() -> None:
    output = """\
2K performance run parameters for coremark.
Iterations        : 1000000
Total time (secs): 10.00
seedcrc           : 0xe9f5
[0]crclist        : 0xe714
[0]crcmatrix      : 0x1fd7
[0]crcstate       : 0x8e3a
[0]crcfinal       : 0x988c
Correct operation validated. See README.md for run and reporting rules.
CoreMark 1.0 : 123.45 / GCC
"""
    validator = ROOT / "extbench/coremark/verify.py"
    assert run(validator, output, ["0", "0", "0", "1000000"]).returncode == 0
    assert run(validator, output.replace("0xe714", "0x0000"), ["0", "0", "0", "1000000"]).returncode != 0
    assert run(validator, output.replace("0x988c", "0x1234"), ["0", "0", "0", "1000000"]).returncode != 0
    assert run(validator, output, ["0", "0", "0", "100"]).returncode != 0
    short_output = output.replace("10.00", "1.00").replace(
        "Correct operation validated. See README.md for run and reporting rules.\n", ""
    ).replace("CoreMark 1.0 : 123.45 / GCC\n", "") + "ERROR! Must execute for at least 10 secs for a valid result!\nErrors detected\n"
    assert run(validator, short_output, ["0", "0", "0", "1000000"]).returncode == 0
    assert run(validator, short_output.replace("0x988c", "0x1234"), ["0", "0", "0", "1000000"]).returncode != 0
    assert run(validator, short_output + "runtime error\n", ["0", "0", "0", "1000000"]).returncode != 0
    assert run(validator, output + "runtime error\n", ["0", "0", "0", "1000000"]).returncode != 0
    assert run(validator, output.replace("Correct operation validated. See README.md for run and reporting rules.\n", ""), ["0", "0", "0", "1000000"]).returncode != 0
    assert run(validator, output.replace("[0]crcfinal       : 0x988c\n", ""), ["0", "0", "0", "1000000"]).returncode != 0


def test_dhrystone() -> None:
    expected = (ROOT / "extbench/dhrystone/expected.out").read_text()
    output = "Execution starts, 100 runs through Dhrystone\n\n"
    output += "Final values of the variables used in the benchmark:\n" + expected.replace(
        "100000010", "110"
    ).replace("<pointer>", "0x1234")
    output += "Measured time 0.01 sec\n"
    validator = ROOT / "extbench/dhrystone/verify.py"
    assert run(validator, output, ["100"]).returncode == 0
    assert run(validator, output.replace("110", "111", 1), ["100"]).returncode != 0
    assert run(validator, output, ["101"]).returncode != 0


if __name__ == "__main__":
    test_coremark()
    test_dhrystone()
    print("external benchmark validators pass")
