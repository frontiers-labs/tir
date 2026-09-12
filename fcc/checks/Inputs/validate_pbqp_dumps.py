#!/usr/bin/env python3

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


INF_COST = 4611686018427387903
U64_MAX = 18446744073709551615


def compile_source(fcc, source, output, dump_dir, cwd=None):
    env = os.environ.copy()
    if dump_dir is None:
        env.pop("TIR_PBQP_DUMP_DIR", None)
    else:
        env["TIR_PBQP_DUMP_DIR"] = str(dump_dir)
    return subprocess.run(
        [fcc, "compile", "--stage", "asm", "--march", "riscv64", source, "-o", output],
        env=env,
        capture_output=True,
        cwd=cwd,
    )


def validate_dump(path):
    data = json.loads(path.read_text())
    assert set(data) == {
        "format",
        "version",
        "kind",
        "inf_cost",
        "node_costs",
        "edges",
        "matrices",
    }
    assert data["format"] == "tir-pbqp"
    assert data["version"] == 1
    assert data["kind"] in {"isel", "regalloc"}
    assert path.name.startswith(data["kind"] + "-")
    assert data["inf_cost"] == INF_COST

    for costs in data["node_costs"]:
        assert costs
        assert all(type(cost) is int and 0 <= cost <= U64_MAX for cost in costs)

    pairs = [(edge["lhs"], edge["rhs"]) for edge in data["edges"]]
    assert pairs == sorted(pairs)
    assert len(pairs) == len(set(pairs))
    matrix_ids = []
    for edge in data["edges"]:
        assert 0 <= edge["lhs"] < edge["rhs"] < len(data["node_costs"])
        assert 0 <= edge["matrix"] < len(data["matrices"])
        if edge["matrix"] not in matrix_ids:
            matrix_ids.append(edge["matrix"])
    assert matrix_ids == list(range(len(data["matrices"])))

    for matrix in data["matrices"]:
        assert matrix["rows"] * matrix["cols"] == len(matrix["costs"])
        assert all(type(cost) is int and 0 <= cost <= U64_MAX for cost in matrix["costs"])
    for edge in data["edges"]:
        matrix = data["matrices"][edge["matrix"]]
        assert matrix["rows"] == len(data["node_costs"][edge["lhs"]])
        assert matrix["cols"] == len(data["node_costs"][edge["rhs"]])
    return data["kind"]


def capture(fcc, source):
    with tempfile.TemporaryDirectory(prefix="tir-pbqp-dumps-") as directory:
        root = Path(directory)
        dump_dir = root / "dumps"
        result = compile_source(fcc, source, root / "output.s", dump_dir)
        assert result.returncode == 0, result.stderr.decode()
        dumps = sorted(dump_dir.glob("*.json"))
        assert len(dumps) > 1
        assert all(re.fullmatch(r"(isel|regalloc)-\d+-\d+\.json", path.name) for path in dumps)
        assert {validate_dump(path) for path in dumps} == {"isel", "regalloc"}


def disabled(fcc, source):
    with tempfile.TemporaryDirectory(prefix="tir-pbqp-disabled-") as directory:
        root = Path(directory)
        for index, dump_dir in enumerate([None, ""]):
            result = compile_source(
                fcc, source, root / f"output-{index}.s", dump_dir, cwd=root
            )
            assert result.returncode == 0, result.stderr.decode()
            assert result.stderr == b""
            assert list(root.glob("*.json")) == []


def output_unchanged(fcc, source):
    with tempfile.TemporaryDirectory(prefix="tir-pbqp-output-") as directory:
        root = Path(directory)
        baseline = compile_source(fcc, source, "-", None)
        dumped = compile_source(fcc, source, "-", root / "dumps")
        assert baseline.returncode == 0, baseline.stderr.decode()
        assert dumped.returncode == 0, dumped.stderr.decode()
        assert dumped.stdout == baseline.stdout


def preserve_existing(fcc, source):
    with tempfile.TemporaryDirectory(prefix="tir-pbqp-preserve-") as directory:
        root = Path(directory)
        dump_dir = root / "dumps"
        first = compile_source(fcc, source, root / "first.s", dump_dir)
        assert first.returncode == 0, first.stderr.decode()
        existing = {path: path.read_bytes() for path in dump_dir.glob("*.json")}
        assert existing

        sentinel = dump_dir / "existing.txt"
        sentinel.write_text("keep me")
        second = compile_source(fcc, source, root / "second.s", dump_dir)
        assert second.returncode == 0, second.stderr.decode()
        assert sentinel.read_text() == "keep me"
        assert all(path.read_bytes() == contents for path, contents in existing.items())
        assert len(list(dump_dir.glob("*.json"))) > len(existing)


def invalid_path(fcc, source):
    with tempfile.TemporaryDirectory(prefix="tir-pbqp-invalid-") as directory:
        root = Path(directory)
        invalid = root / "not-a-directory"
        invalid.write_text("occupied")
        result = compile_source(fcc, source, root / "output.s", invalid)
        assert result.returncode == 0, result.stderr.decode()
        warning = result.stderr.decode()
        assert "tir-pbqp: cannot create dump directory" in warning
        assert str(invalid) in warning
        assert (root / "output.s").is_file()
        assert invalid.read_text() == "occupied"


def concurrent(fcc, source):
    with tempfile.TemporaryDirectory(prefix="tir-pbqp-concurrent-") as directory:
        root = Path(directory)
        dump_dir = root / "dumps"
        env = os.environ.copy()
        env["TIR_PBQP_DUMP_DIR"] = str(dump_dir)
        processes = [
            subprocess.Popen(
                [
                    fcc,
                    "compile",
                    "--stage",
                    "asm",
                    "--march",
                    "riscv64",
                    source,
                    "-o",
                    root / f"output-{index}.s",
                ],
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            for index in range(2)
        ]
        for process in processes:
            _, stderr = process.communicate()
            assert process.returncode == 0, stderr.decode()

        dumps = list(dump_dir.glob("*.json"))
        assert len(dumps) >= 4
        assert len({path.name for path in dumps}) == len(dumps)
        assert {validate_dump(path) for path in dumps} == {"isel", "regalloc"}


def main():
    scenario, fcc, source = sys.argv[1:]
    scenarios = {
        "capture": capture,
        "disabled": disabled,
        "output": output_unchanged,
        "preserve": preserve_existing,
        "invalid-path": invalid_path,
        "concurrent": concurrent,
    }
    scenarios[scenario](fcc, source)


if __name__ == "__main__":
    main()
