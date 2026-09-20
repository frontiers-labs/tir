#!/usr/bin/env python3
"""Check complete PDL examples in the book with a built tir-pdl executable."""

import argparse
from pathlib import Path
import re
import subprocess
import tempfile


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--compiler", type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parents[3]
compiler = args.compiler or root / "target/debug/tir-pdl"
pages = [root / "docs/design/instcombine.md", *sorted((root / "docs/pdl").glob("*.md"))]
count = 0
failed = False
with tempfile.TemporaryDirectory(prefix="pdl-docs-") as directory:
    for page in pages:
        source = page.read_text()
        for example in re.finditer(r"^```pdl\n(.*?)^```[ \t]*$", source, re.M | re.S):
            line = source.count("\n", 0, example.start()) + 1
            path = Path(directory) / f"{page.stem}-{line}.pdl"
            path.write_text(example[1])
            result = subprocess.run(
                [str(compiler), str(path), "--emit", "ast"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            count += 1
            if result.returncode:
                failed = True
                print(f"{page.relative_to(root)}:{line}")
                print(result.stderr)
print(f"Checked {count} PDL examples.")
raise SystemExit(1 if failed or count == 0 else 0)
