#!/usr/bin/env python3
"""Fail when the workspace gains a direct production dependency.

A production dependency is an external crate listed under `[dependencies]` or
`[build-dependencies]` of any workspace member. Dev-dependencies and in-tree
crates do not count. `production-dependencies.txt` holds the allowed set, and
a removed dependency must leave it, so the set can only shrink.
"""

import json
import subprocess
import sys
from pathlib import Path


ALLOWED = Path(__file__).with_name("production-dependencies.txt")


def main():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"]))
    actual = {
        dependency["name"]
        for package in metadata["packages"]
        for dependency in package["dependencies"]
        if dependency["kind"] != "dev" and "path" not in dependency
    }
    allowed = set(ALLOWED.read_text().split())
    for name in sorted(actual - allowed):
        print(f"error: new production dependency `{name}`; implement it in-tree instead")
    for name in sorted(allowed - actual):
        print(f"error: `{name}` is no longer a production dependency; remove it from {ALLOWED.name}")
    return actual != allowed


if __name__ == "__main__":
    sys.exit(main())
