#!/usr/bin/env python3
"""Regenerate FileCheck `CHECK` lines for LIT-style tests.

This is the generic successor to the old, tmdl-only ``update_tmdlc_checks.py``.
It works for any module whose tests follow the convention used across the TIR
repository: a test file contains one or more ``RUN:`` lines of the form::

    // RUN: <tool> <args...> | filecheck %s

The script re-runs the command (everything up to the first ``| filecheck``),
captures its standard output and rewrites the ``CHECK``/``CHECK-NEXT`` lines so
they match. Absolute paths under the repository root are replaced with ``{{.*}}``
regex blocks so the generated checks are machine independent.

Usage::

    ./utils/scripts/update_checks.py <module> [file ...]

``module`` is one of the keys in ``MODULES`` below (e.g. ``tmdl`` or ``fcc``).
When no files are given, every test file with a ``RUN:`` line under the
module's checks directory is regenerated.
"""

import argparse
import os
import re
import subprocess
import sys

# Per-module configuration. ``dir`` is relative to the repository root and
# ``comment`` is the line-comment token used to emit directives.
MODULES = {
    "tmdl": {"dir": "tmdl/checks", "comment": "//"},
    "fcc": {"dir": "fcc/checks", "comment": "//"},
    # Backends are wired up but not yet generating checks; kept here so the
    # tooling is ready when a backend tool lands.
    "riscv": {"dir": "backends/riscv/checks", "comment": "#"},
}

GENERATED_HEADER = (
    "This file was generated with ./utils/scripts/update_checks.py. "
    "Do not modify CHECKs manually."
)

# Substring identifying any previously generated header line, regardless of the
# script name that produced it.
GENERATED_MARKER = "This file was generated"

CHECK_RE = re.compile(r"^\s*(//|#)\s*CHECK")
RUN_RE = re.compile(r"RUN:\s*(.*)$")


def repo_root():
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], stderr=subprocess.DEVNULL
        )
        return out.decode("utf-8").strip()
    except Exception:
        return os.getcwd()


def find_target_dir(root):
    for candidate in ("target/debug", "target/release"):
        if os.path.isdir(os.path.join(root, candidate)):
            return os.path.join(root, candidate)
    return os.path.join(root, "target/debug")


def extract_run_commands(lines):
    commands = []
    for line in lines:
        m = RUN_RE.search(line)
        if m:
            commands.append(m.group(1).strip())
    return commands


def resolve_program(token, target_dir):
    """Resolve a tool name to its built binary inside the target directory."""
    candidate = os.path.join(target_dir, token)
    if os.path.isfile(candidate):
        return candidate
    return token


def run_tool(command, test_path, target_dir):
    """Run the part of a RUN pipeline before ``| filecheck`` and return stdout."""
    pre_filecheck = command.split("|")[0].strip()
    tokens = pre_filecheck.split()
    substituted = [
        tok.replace("%s", test_path).replace("%S", os.path.dirname(test_path))
        for tok in tokens
    ]
    if substituted and substituted[0] == "not":
        substituted = substituted[1:]
    substituted[0] = resolve_program(substituted[0], target_dir)
    return subprocess.check_output(substituted).decode("utf-8")


def normalize(line, root):
    """Replace absolute repository paths with a ``{{.*}}`` regex block."""
    return line.replace(root, "{{.*}}")


def generate_checks(output, comment, root):
    checks = []
    check_next = False
    for line in output.splitlines():
        if line.strip() == "":
            checks.append(f"{comment} CHECK-EMPTY:")
            check_next = False
            continue
        text = normalize(line, root)
        directive = "CHECK-NEXT" if check_next else "CHECK"
        checks.append(f"{comment} {directive}: {text}")
        check_next = True
    return checks


def process_file(path, module_cfg, root, target_dir):
    comment = module_cfg["comment"]
    with open(path, "r") as f:
        lines = f.read().splitlines()

    commands = extract_run_commands(lines)
    if not commands:
        return False

    # Preamble: everything that is not a generated header and not a CHECK line.
    preamble = [
        line
        for line in lines
        if GENERATED_MARKER not in line and not CHECK_RE.match(line)
    ]
    while preamble and preamble[0].strip() == "":
        preamble.pop(0)
    while preamble and preamble[-1].strip() == "":
        preamble.pop()

    checks = []
    for command in commands:
        output = run_tool(command, path, target_dir)
        checks.extend(generate_checks(output, comment, root))

    with open(path, "w") as f:
        f.write(f"{comment} {GENERATED_HEADER}\n\n")
        for line in preamble:
            f.write(line + "\n")
        f.write("\n")
        for line in checks:
            f.write(line + "\n")

    return True


def collect_test_files(checks_dir):
    """Find auto-generated test files under ``checks_dir``.

    Only files that already carry the generated header are returned, so a bulk
    update never clobbers hand-authored CHECK directives (e.g. tests that rely
    on ``CHECK-COUNT`` or selective matching). To seed a new golden test, run
    the script with its path passed explicitly.
    """
    files = []
    for dirpath, _dirnames, filenames in os.walk(checks_dir):
        if os.path.basename(dirpath) == "Inputs":
            continue
        for name in filenames:
            full = os.path.join(dirpath, name)
            with open(full, "r", errors="ignore") as f:
                contents = f.read()
            if "RUN:" in contents and GENERATED_MARKER in contents:
                files.append(full)
    return sorted(files)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("module", choices=sorted(MODULES), help="module to update")
    parser.add_argument("files", nargs="*", help="specific test files (optional)")
    args = parser.parse_args()

    root = repo_root()
    target_dir = find_target_dir(root)
    module_cfg = MODULES[args.module]

    if args.files:
        files = [os.path.abspath(f) for f in args.files]
    else:
        files = collect_test_files(os.path.join(root, module_cfg["dir"]))

    if not files:
        print(f"No test files found for module '{args.module}'.", file=sys.stderr)
        return

    for path in files:
        updated = process_file(path, module_cfg, root, target_dir)
        status = "updated" if updated else "skipped (no RUN line)"
        print(f"{status}: {os.path.relpath(path, root)}")


if __name__ == "__main__":
    main()
