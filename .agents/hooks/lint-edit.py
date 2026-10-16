#!/usr/bin/env python3
"""PostToolUse hook for coding agents: run the ast-grep rules on the Rust
files an edit touched and hand any finding back to the agent.

Edit and Write tools name the file in `tool_input.file_path`; Codex's
`apply_patch` carries a patch in `tool_input.command` with `*** Add File:`
and `*** Update File:` headers.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

def edited_files(event):
    tool_input = event.get("tool_input") or {}
    if "file_path" in tool_input:
        return [tool_input["file_path"]]
    patch = tool_input.get("command") or ""
    if isinstance(patch, list):
        patch = "\n".join(patch)
    return re.findall(r"^\*\*\* (?:Add|Update) File: (.+)$", patch, re.MULTILINE)


def main():
    event = json.load(sys.stdin)
    root = Path(__file__).resolve().parents[2]
    files = [f for f in edited_files(event) if f.endswith(".rs") and (root / f).exists()]
    if not files:
        return 0
    result = subprocess.run([".github/scripts/check-ast-grep.py", *files],
                            cwd=root, capture_output=True, text=True)
    if result.returncode == 0:
        return 0
    sys.stderr.write(result.stdout + result.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
