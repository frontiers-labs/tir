"""Lines a list file gained against master. Accepted-findings lists use it to
make sure they only shrink."""

import collections
import os
import subprocess


def added_lines(root, path):
    base = os.environ.get("LINT_BASE", "master")
    subprocess.run(["git", "rev-parse", "--verify", "--quiet", base], cwd=root, check=True,
                   stdout=subprocess.DEVNULL)
    before = subprocess.run(["git", "show", f"{base}:{path}"], cwd=root, capture_output=True,
                            text=True)
    if before.returncode != 0:
        # The list is new on this branch, so everything in it is its baseline.
        return []
    gained = (collections.Counter((root / path).read_text().splitlines())
              - collections.Counter(before.stdout.splitlines()))
    return sorted(gained.elements())
