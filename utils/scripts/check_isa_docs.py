#!/usr/bin/env python3
"""Check that mdBook rendered every generated ISA chapter after cargo xtask docs."""

from pathlib import Path
import sys

root = Path(__file__).resolve().parents[2]
sources = sorted((root / "docs/generated/isa").rglob("*.md"))
if not sources:
    sys.exit("No generated ISA chapters found. Run cargo xtask docs first.")

missing = [
    source.relative_to(root / "docs").with_suffix(".html")
    for source in sources
    if not (root / "book" / source.relative_to(root / "docs")).with_suffix(".html").is_file()
]
if missing:
    sys.exit("ISA chapters missing from the rendered book:\n" + "\n".join(map(str, missing)))

print(f"All {len(sources)} generated ISA chapters were rendered.")
