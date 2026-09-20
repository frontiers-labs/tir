#!/usr/bin/env python3
"""Check manual links and bit diagrams in the book built by cargo xtask docs."""

from pathlib import Path
import re
import xml.etree.ElementTree as ET


root = Path(__file__).resolve().parents[2]
targets = {
    "riscv": "backends/riscv/defs",
    "aarch64": "backends/arm64/defs",
    "x86-64": "backends/x86_64/defs",
    "ptx": "gpu/defs/ptx",
}
namespace = {"s": "http://www.w3.org/2000/svg"}
declarations = diagrams = 0

for target, source_dir in targets.items():
    book_dir = root / "book/generated/isa" / target
    overview = (book_dir / "index.html").read_text()
    for source in (root / source_dir).rglob("*.tmdl"):
        for kind, name in re.findall(r"^(isa|register_class) (\w+)", source.read_text(), re.M):
            section = re.search(
                rf"<h3\b[^>]*>.*?<code>{re.escape(name)}</code>.*?</h3>(.*?)(?=<h[23]\b|\Z)",
                overview,
                re.S,
            )
            assert section, f"{source}: {kind} {name} missing from {book_dir}/index.html"
            assert re.search(r'<p>.*?<a href="https://', section[1], re.S), (
                f"{source}: {kind} {name} has no rendered manual link"
            )
            declarations += 1

    target_diagrams = 0
    for page in book_dir.glob("*.html"):
        for markup in re.findall(r"<svg\b.*?</svg>", page.read_text(), re.S):
            svg = ET.fromstring(markup)
            if not svg.get("aria-label", "").endswith("-bit instruction encoding"):
                continue
            total_bits = int(svg.get("aria-label").split("-")[0])
            width = float(svg.get("viewBox", svg.get("viewbox")).split()[2]) - 2
            bit_width = width / total_bits
            next_bit = total_bits - 1
            for field in svg.findall("s:g", namespace):
                title = field.find("s:title", namespace).text
                match = re.fullmatch(r"Bits (\d+):(\d+): (.+)", title)
                assert match, f"{page}: missing field description: {title}"
                end, start = map(int, match.group(1, 2))
                assert end == next_bit and start <= end, f"{page}: gap or overlap at {title}"
                rect = field.find("s:rect", namespace)
                assert float(rect.get("width")) == (end - start + 1) * bit_width, (
                    f"{page}: field width is not proportional at {title}"
                )
                assert float(rect.get("x")) == 1 + (total_bits - 1 - end) * bit_width, (
                    f"{page}: field position is wrong at {title}"
                )
                next_bit = start - 1
            assert next_bit == -1, f"{page}: incomplete {total_bits}-bit diagram"
            target_diagrams += 1
    if target == "ptx":
        assert target_diagrams == 0, "PTX must not invent binary encodings"
    else:
        assert target_diagrams, f"{book_dir}: no instruction diagrams rendered"
    diagrams += target_diagrams

print(f"Checked {declarations} documented declarations and {diagrams} instruction diagrams.")
