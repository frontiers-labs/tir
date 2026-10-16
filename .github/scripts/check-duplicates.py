#!/usr/bin/env python3
"""Fail when two functions of more than ten lines are near copies.

dryer (https://github.com/unclebob/dryer) finds the pairs by comparing
normalized syntax trees. It has no way to accept a pair, so
`.config/duplicates.allow` lists the accepted ones by function, each with the
reason it stays. An entry that no longer matches must leave the list, and the
list may not gain lines against master.
"""

import re
import subprocess
import sys
from pathlib import Path

from shrinking import added_lines

DRYER = "git+https://github.com/unclebob/dryer@66ff6d21a42c04afcad89c78a80066176d1294b0"
MIN_LINES = "11"
# dryer drops field names, so below this two unrelated field-by-field struct
# conversions look alike.
THRESHOLD = "0.85"

ROOT = Path(__file__).resolve().parents[2]
ALLOWED = Path(".config/duplicates.allow")
PAIR = re.compile(r"^DUPLICATE score=(\S+)\n\s+(\S+):(\d+)-\d+\n\s+(\S+):(\d+)-\d+", re.MULTILINE)


def function_at(path, line):
    lines = (ROOT / path).read_text().splitlines()[line - 1:line + 3]
    name = re.search(r"\bfn\s+(\w+)", "\n".join(lines))
    return f"{path}::{name.group(1) if name else line}"


def allowed_pairs():
    pairs = set()
    for line in (ROOT / ALLOWED).read_text().splitlines():
        entry = line.split("#", 1)[0].split()
        if entry:
            pairs.add(frozenset(entry))
    return pairs


def main():
    report = subprocess.run(
        ["uvx", "--quiet", "--from", DRYER, "dryer",
         "--min-lines", MIN_LINES, "--threshold", THRESHOLD, "."],
        cwd=ROOT, check=True, capture_output=True, text=True).stdout
    allowed = allowed_pairs()
    seen = set()
    failed = False
    for score, left, left_line, right, right_line in PAIR.findall(report):
        pair = frozenset((function_at(left, int(left_line)), function_at(right, int(right_line))))
        seen.add(pair)
        if pair not in allowed:
            failed = True
            print(f"error: near copies (score {score}): {left}:{left_line} and {right}:{right_line}; "
                  "merge them into one")
    for pair in sorted(allowed - seen, key=sorted):
        failed = True
        print(f"error: {' '.join(sorted(pair))} are no longer near copies; "
              f"remove them from {ALLOWED}")
    for line in added_lines(ROOT, ALLOWED):
        failed = True
        print(f"error: {ALLOWED} may only shrink; new line: {line}")
    return failed


if __name__ == "__main__":
    sys.exit(main())
