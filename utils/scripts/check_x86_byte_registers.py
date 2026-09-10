#!/usr/bin/env python3
"""Check mixed byte registers against GNU as. Usage: check_x86_byte_registers.py TIR"""

import itertools
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def run(*args):
    return subprocess.run(args, capture_output=True, text=True, check=True).stdout


def main():
    tir = str(Path(sys.argv[1]).resolve())
    high = ("ah", "ch", "dh", "bh")
    low = ("al", "cl", "dl", "bl")
    rex = ("spl", "bpl", "sil", "dil", *(f"r{i}b" for i in range(8, 16)))
    mnemonics = ("mov", "add", "sub", "and", "or", "xor")
    valid = [
        f"{mnemonic} {dst}, {src}"
        for mnemonic, hi, lo in itertools.product(mnemonics, high, low)
        for dst, src in ((hi, lo), (lo, hi))
    ]
    with tempfile.TemporaryDirectory() as directory:
        directory = Path(directory)
        source = directory / "mixed.S"
        reference = directory / "reference.S"
        obj = directory / "reference.o"
        raw = directory / "reference.bin"
        assembly = ".global mixed\nmixed:\n" + "\n".join(valid) + "\n"
        source.write_text(assembly)
        reference.write_text(".intel_syntax noprefix\n" + assembly)
        run("as", "--64", "-o", str(obj), str(reference))
        run("objcopy", "-O", "binary", "--only-section=.text", str(obj), str(raw))
        output = run(tir, "mc", "--march=x86_64", "--filetype=obj-ascii", str(source))
        encoded = bytes(int(byte, 16) for byte in re.findall(r"0x([0-9A-F]{2})\b", output))
        assert encoded == raw.read_bytes(), "mixed byte encodings differ from GNU as"
        print(f"{len(valid)} mixed byte encodings match GNU as", flush=True)

        rejected = 0
        for mnemonic, hi, lo in itertools.product(mnemonics, high, rex):
            for dst, src in ((hi, lo), (lo, hi)):
                instruction = f"{mnemonic} {dst}, {src}"
                source.write_text(f".global invalid\ninvalid:\n{instruction}\n")
                result = subprocess.run(
                    [tir, "mc", "--march=x86_64", "--stage=isel", str(source)],
                    capture_output=True,
                    text=True,
                )
                assert result.returncode == 1, f"accepted {instruction}: {result.stdout}"
                assert f"invalid operands for instruction '{mnemonic}'" in result.stderr
                rejected += 1
        print(f"{rejected} high-byte/REX pairs rejected during assembly")


if __name__ == "__main__":
    main()
