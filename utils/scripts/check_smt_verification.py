#!/usr/bin/env python3
"""Run Sail equivalence regressions through the verification CLI."""

import json
import os
import re
import tempfile
from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[2]


def check(isa, instructions, unsupported=()):
    result = subprocess.run(
        ["cargo", "xtask", "verify", isa],
        cwd=ROOT,
        env={**os.environ, "TIR_VERIFY_SMT_FILTER": ",".join([*instructions, *unsupported])},
        check=False,
    )
    assert result.returncode == 0, f"{isa} verification failed"
    report = json.loads((ROOT / f"target/verify/smt/{isa}/report.json").read_text())
    checked = {entry["instruction"] for entry in report["instructions"]}
    assert checked == set(instructions), f"unchecked instructions: {set(instructions) - checked}"
    skipped = {entry.split()[0] for entry in report["unsupported"]}
    assert skipped == set(unsupported), f"unexpected unsupported instructions: {skipped}"
    assert report["verified"] > 0
    assert report["failed"] == 0
    assert report["unknown"] == 0
    for entry in report["instructions"]:
        assert entry["shape_cases"], entry["instruction"]
        assert all(count > 0 for _, count in entry["shape_cases"]), entry["instruction"]


def check_counterexample():
    queries = ROOT / "target/verify/smt/x86_64/queries"
    for path in sorted(queries.glob("sarcl_00f8d348_p*.smt2")):
        query = path.read_text()
        if "(exists " not in query:
            continue
        result = subprocess.run(["z3", "-T:20", str(path)], capture_output=True, text=True)
        if result.stdout.splitlines()[0] != "unsat":
            continue
        query, count = re.subn(
            r"(\(define-fun st1_gpr \(\) \(Array \(_ BitVec 4\) \(_ BitVec 64\)\) )(.*)\)\n",
            lambda match: match[1] + "(store " + match[2]
            + " (_ bv15 4) (bvnot (select st0_gpr (_ bv15 4)))))\n",
            query,
        )
        assert count == 1
        with tempfile.NamedTemporaryFile(mode="w", suffix=".smt2") as mutated:
            mutated.write(query)
            mutated.flush()
            result = subprocess.run(["z3", "-T:20", mutated.name], capture_output=True, text=True)
        assert result.stdout.splitlines()[0] == "sat", result.stdout
        return
    raise AssertionError("no verified sarcl query with undefined flag choices")


if __name__ == "__main__":
    check(
        "x86_64",
        [
            "setparity", "setnoparity", "sarcl", "leabaseindex",
            "movsw", "cmpsq", "shlimm8", "sarcl16", "rorimm",
            "imul16memorysourcedisp", "imul32memorysourcedisp", "imulmemorysourcedisp",
        ],
        unsupported=[
            "andn", "btr", "rorx", "unsigneddivide32",
            "blsr", "blsr32", "blsmsk32", "bzhi", "mulx32", "shrx32",
            "pushf", "signeddivide32",
            "shldimm", "shrdimm", "shldcl", "shrdcl",
        ],
    )
    check_counterexample()
    check("riscv32", ["loadword", "storeword", "bitset", "readfenv", "setfenv"])
    check("riscv64", ["bitset", "readfenv", "setfenv"])
    check(
        "armv8",
        [
            "loadacquire", "storerelease", "subword",
            "addvector16b", "subvector16b", "andvector16b",
        ],
    )
