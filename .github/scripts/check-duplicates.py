#!/usr/bin/env -S uv run --script --quiet
# /// script
# dependencies = ["dryer @ git+https://github.com/unclebob/dryer@66ff6d21a42c04afcad89c78a80066176d1294b0"]
# ///
"""Fail when two functions of more than ten lines are near copies.

dryer (https://github.com/unclebob/dryer) normalizes each function's syntax
tree and scores pairs by the Jaccard similarity of their fingerprints. Its CLI
scores every pair, which takes minutes here, so this script parses with dryer
and runs the exact prefix-filtered all-pairs join of Bayardo et al., "Scaling
Up All Pairs Similarity Search" (WWW 2007), scoring each candidate with dryer.

It has no way to accept a pair, so `.config/duplicates.allow` lists the
accepted ones by function, each with the reason it stays. An entry that no
longer matches must leave the list, and the list may not gain lines against
master.
"""

import collections
import math
import multiprocessing
import re
import sys
from pathlib import Path

from dryer.discover import iter_source_files
from dryer.scan import scan_files
from dryer.shape import jaccard

from shrinking import added_lines

MIN_LINES = 11
MIN_NODES = 20
# dryer drops field names, so below this two unrelated field-by-field struct
# conversions look alike.
THRESHOLD = 0.85

ROOT = Path(__file__).resolve().parents[2]
ALLOWED = Path(".config/duplicates.allow")


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


def entries_of(path):
    return scan_files([path], ROOT, MIN_LINES, MIN_NODES)[0]


def near_copies(entries):
    """Pairs of same-language entries at or above THRESHOLD.

    Two sets of sizes m <= n with Jaccard similarity t share at least t * n
    elements, so their prefixes of n - floor(t * n) + 1 and m - floor(t * m) + 1
    rarest elements meet. Indexing only those prefixes leaves few candidates to
    score.
    """
    frequency = collections.Counter(f for entry in entries for f in entry.fingerprints)
    rank = {f: i for i, f in enumerate(sorted(frequency, key=lambda f: (frequency[f], f)))}
    entries = sorted(entries, key=lambda entry: len(entry.fingerprints))
    index = collections.defaultdict(list)
    for position, entry in enumerate(entries):
        size = len(entry.fingerprints)
        prefix = sorted(rank[f] for f in entry.fingerprints)[:size - math.floor(THRESHOLD * size) + 1]
        for other in map(entries.__getitem__, {o for token in prefix for o in index[token]}):
            same_span = ((other.file, other.start_line, other.end_line)
                         == (entry.file, entry.start_line, entry.end_line))
            if (other.language == entry.language and not same_span
                    and jaccard(other.fingerprints, entry.fingerprints) >= THRESHOLD):
                yield other, entry
        for token in prefix:
            index[token].append(position)


def main():
    with multiprocessing.Pool() as pool:
        entries = [e for found in pool.map(entries_of, iter_source_files([ROOT])) for e in found]
    allowed = allowed_pairs()
    seen = set()
    failed = False
    for left, right in near_copies(entries):
        pair = frozenset((function_at(left.file, left.start_line),
                          function_at(right.file, right.start_line)))
        seen.add(pair)
        if pair not in allowed:
            failed = True
            print(f"error: near copies: {left.file}:{left.start_line} and "
                  f"{right.file}:{right.start_line}; merge them into one")
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
