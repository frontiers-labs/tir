#!/usr/bin/env python3
"""Run the ast-grep rules in .config/ast-grep and fail on new findings.

`.config/ast-grep/baseline.tsv` lists the findings that predate their rule,
one per line as rule, file and the trimmed first line of the match. Any other
finding fails. A listed finding that is gone fails until its line leaves the
baseline, and the baseline may not gain lines against master. File arguments
limit the scan to those files; without them the rule tests run first.
"""

import collections
import json
import subprocess
import sys
from pathlib import Path

from shrinking import added_lines

AST_GREP = "ast-grep-cli==0.51.0"
ROOT = Path(__file__).resolve().parents[2]
CONFIG = ".config/ast-grep/sgconfig.yml"
BASELINE = Path(".config/ast-grep/baseline.tsv")


def ast_grep(*args, **kwargs):
    return subprocess.run(["uvx", "--quiet", "--from", AST_GREP, "ast-grep", *args, "-c", CONFIG],
                          cwd=ROOT, text=True, **kwargs)


def key(rule, path, line):
    return "\t".join((rule, path, " ".join(line.split())))


def main(paths):
    if not paths:
        ast_grep("test", "--skip-snapshot-tests", check=True)
    paths = [str(Path(p).resolve().relative_to(ROOT)) for p in paths]
    scan = ast_grep("scan", "--json=stream", *(paths or ["."]), capture_output=True)
    found = collections.Counter()
    where = {}
    for match in map(json.loads, scan.stdout.splitlines()):
        entry = key(match["ruleId"], match["file"], match["lines"].splitlines()[0])
        found[entry] += 1
        where[entry] = f"{match['file']}:{match['range']['start']['line'] + 1}: {match['message']}"

    listed = collections.Counter((ROOT / BASELINE).read_text().splitlines())
    if paths:
        listed = collections.Counter({e: n for e, n in listed.items() if e.split("\t")[1] in paths})
    failed = False
    for entry in sorted(found - listed):
        failed = True
        rule = entry.split("\t")[0]
        print(f"error[{rule}]: {where[entry]}")
    for entry in sorted((listed - found).elements()):
        failed = True
        print(f"error: fixed, remove from {BASELINE}: {entry}")
    for entry in added_lines(ROOT, BASELINE):
        failed = True
        print(f"error: {BASELINE} may only shrink; new line: {entry}")
    return failed


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
