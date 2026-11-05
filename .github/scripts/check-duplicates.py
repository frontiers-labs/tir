#!/usr/bin/env python3
"""Fail on unaccepted duplicate clusters of functions over ten lines.

dupehound clusters transitively, so `.config/dupehound.allow` accepts pairs
of cluster members, without asserting each pair meets the similarity threshold.
Every pair must be listed. Entries that no longer share a cluster must leave
the list, and the list may not gain lines against master.
"""

import itertools
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from shrinking import added_lines

VERSION = "0.1.2"
MIN_LINES = 11
THRESHOLD = 0.85

ROOT = Path(__file__).resolve().parents[2]
ALLOWED = Path(".config/dupehound.allow")


def scan():
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "tir/dupehound" / VERSION
    binary = cache / "bin/dupehound"
    if not binary.is_file():
        with tempfile.TemporaryDirectory(prefix="tir-dupehound-") as build:
            subprocess.run(["cargo", "install", "dupehound", "--version", f"={VERSION}", "--locked",
                            "--no-default-features", "--root", cache, "--target-dir", build],
                           cwd=ROOT, check=True)
    result = subprocess.run([binary, "scan", ".", "--json", "--threshold", str(THRESHOLD),
                             "--exclude", "**/checks/**"],
                            cwd=ROOT, text=True, stdout=subprocess.PIPE, check=True)
    report = json.loads(result.stdout)
    if report["schema_version"] != 1:
        raise ValueError(f"unsupported dupehound JSON schema: {report['schema_version']}")
    return report["clusters"]


def allowed_pairs():
    pairs = set()
    for line in (ROOT / ALLOWED).read_text().splitlines():
        entry = line.split("#", 1)[0].split()
        if entry:
            pairs.add(frozenset(entry))
    return pairs


def main():
    allowed = allowed_pairs()
    seen = set()
    failed = False
    for cluster in scan():
        members = [m for m in cluster["members"]
                   if m["end_line"] - m["start_line"] + 1 >= MIN_LINES]
        for left, right in itertools.combinations(members, 2):
            pair = frozenset(f"{m['file']}::{m['name']}" for m in (left, right))
            seen.add(pair)
            if pair not in allowed:
                failed = True
                print(f"error: unaccepted duplicate-cluster members: "
                      f"{left['file']}:{left['start_line']} ({left['name']}) and "
                      f"{right['file']}:{right['start_line']} ({right['name']})")
    for pair in sorted(allowed - seen, key=sorted):
        failed = True
        print(f"error: {' '.join(sorted(pair))} no longer share a duplicate cluster; "
              f"remove them from {ALLOWED}")
    for line in added_lines(ROOT, ALLOWED):
        failed = True
        print(f"error: {ALLOWED} may only shrink; new line: {line}")
    return failed


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.CalledProcessError, ValueError, KeyError, TypeError) as error:
        sys.exit(f"error: duplicate check failed: {error}")
