#!/usr/bin/env python3
"""Generate scalar RISC-V rounded instruction families and encoding checks."""

import argparse
import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODES = [
    ("RNE", 0, "rne"),
    ("RTZ", 1, "rtz"),
    ("RDN", 2, "rdn"),
    ("RUP", 3, "rup"),
    ("RMM", 4, "rmm"),
    ("", 7, ""),
]


def families():
    for suffix, fmt, width, extension, exponent, mantissa in [
        ("S", 0, 32, "F", 8, 23),
        ("D", 1, 64, "D", 11, 52),
    ]:
        float_register = f"FPR{width}"
        for op, funct in [("Add", 0), ("Sub", 4), ("Mul", 8), ("Div", 12)]:
            yield (
                f"F{op}{suffix}",
                f"f{op.lower()}.{suffix.lower()}",
                extension,
                [
                    ("fd", float_register),
                    ("fs1", float_register),
                    ("fs2", float_register),
                ],
                f"0b{funct + fmt:07b}, fs2, fs1, self.RM, fd, 0b1010011",
                f"f{op.lower()}(fs1, fs2, ROUND)",
                "fd",
                lambda rm, funct=funct, fmt=fmt: (
                    ((funct + fmt) << 25)
                    | (12 << 20)
                    | (11 << 15)
                    | (rm << 12)
                    | (10 << 7)
                    | 0x53
                ),
            )
        for op, opcode, negate_product, negate_addend in [
            ("MAdd", 0x43, False, False),
            ("MSub", 0x47, False, True),
            ("NMSub", 0x4B, True, False),
            ("NMAdd", 0x4F, True, True),
        ]:

            def negate_float(value):
                return (
                    f"asfloat(extract({value}, {width - 1}, 0) ^ "
                    f"(zext(0b1, {width}) << zext(0b{width - 1:b}, {width})))"
                )

            multiplicand = negate_float("fs1") if negate_product else "fs1"
            addend = negate_float("fs3") if negate_addend else "fs3"
            yield (
                f"F{op}{suffix}",
                f"f{op.lower()}.{suffix.lower()}",
                extension,
                [
                    ("fd", float_register),
                    ("fs1", float_register),
                    ("fs2", float_register),
                    ("fs3", float_register),
                ],
                f"fs3, 0b{fmt:02b}, fs2, fs1, self.RM, fd, 0b{opcode:07b}",
                f"fma({multiplicand}, fs2, {addend}, ROUND)",
                "fd",
                lambda rm, fmt=fmt, opcode=opcode: (
                    (13 << 27)
                    | (fmt << 25)
                    | (12 << 20)
                    | (11 << 15)
                    | (rm << 12)
                    | (10 << 7)
                    | opcode
                ),
            )
        yield (
            f"FSqrt{suffix}",
            f"fsqrt.{suffix.lower()}",
            extension,
            [("fd", float_register), ("fs1", float_register)],
            f"0b{44 + fmt:07b}, 0b00000, fs1, self.RM, fd, 0b1010011",
            "sqrt(fs1, ROUND)",
            "fd",
            lambda rm, fmt=fmt: (
                ((44 + fmt) << 25) | (11 << 15) | (rm << 12) | (10 << 7) | 0x53
            ),
        )
        for integer, integer_width, selector, unsigned in [
            ("W", 32, 0, False),
            ("WU", 32, 1, True),
            ("L", 64, 2, False),
            ("LU", 64, 3, True),
        ]:
            feature = extension if integer_width == 32 else extension + "64"
            source = "extract(rs1, 31, 0)" if integer_width == 32 else "rs1"
            builtin = "uitofp" if unsigned else "sitofp"
            yield (
                f"FCvt{suffix}{integer}",
                f"fcvt.{suffix.lower()}.{integer.lower()}",
                feature,
                [("fd", float_register), ("rs1", "GPR")],
                f"0b{104 + fmt:07b}, 0b{selector:05b}, rs1, self.RM, fd, 0b1010011",
                f"{builtin}({source}, {exponent}, {mantissa}, ROUND)",
                "fd",
                lambda rm, fmt=fmt, selector=selector: (
                    ((104 + fmt) << 25)
                    | (selector << 20)
                    | (11 << 15)
                    | (rm << 12)
                    | (10 << 7)
                    | 0x53
                ),
            )
            builtin = "fptoui" if unsigned else "fptosi"
            yield (
                f"FCvt{integer}{suffix}",
                f"fcvt.{integer.lower()}.{suffix.lower()}",
                feature,
                [("rd", "GPR"), ("fs1", float_register)],
                f"0b{96 + fmt:07b}, 0b{selector:05b}, fs1, self.RM, rd, 0b1010011",
                f"{builtin}(fs1, {integer_width}, ROUND)",
                "rd",
                lambda rm, fmt=fmt, selector=selector: (
                    ((96 + fmt) << 25)
                    | (selector << 20)
                    | (11 << 15)
                    | (rm << 12)
                    | (10 << 7)
                    | 0x53
                ),
            )
        other = "D" if suffix == "S" else "S"
        yield (
            f"FCvt{suffix}{other}",
            f"fcvt.{suffix.lower()}.{other.lower()}",
            "D",
            [("fd", float_register), ("fs1", f"FPR{96 - width}")],
            f"0b{32 + fmt:07b}, 0b{1 - fmt:05b}, fs1, self.RM, fd, 0b1010011",
            f"fcvt(fs1, {exponent}, {mantissa}, ROUND)",
            "fd",
            lambda rm, fmt=fmt: (
                ((32 + fmt) << 25)
                | ((1 - fmt) << 20)
                | (11 << 15)
                | (rm << 12)
                | (10 << 7)
                | 0x53
            ),
        )


def behavior(expression, destination, rm, source_width, destination_width, mnemonic):
    dynamic = rm == 7
    rounding = "FRM::frm" if dynamic else f"0b{rm:03b}"
    value = expression.replace("ROUND", rounding)
    result = "value"
    if destination == "rd":
        unsigned = mnemonic.split(".")[1].endswith("u")
        integer_width = 32 if mnemonic.split(".")[1].startswith("w") else 64
        maximum = (1 << (integer_width - int(not unsigned))) - 1
        minimum = 0 if unsigned else 1 << (integer_width - 1)
        bound = maximum if source_width == 64 and integer_width == 32 else maximum + 1
        pack = "f" if source_width == 32 else "d"
        upper = int.from_bytes(struct.pack("<" + pack, float(bound)), "little")
        lower = int.from_bytes(struct.pack("<" + pack, -float(minimum)), "little")
        result = (
            f"if (fs1 >= fs1) == 0b0 {{ zext(0x{maximum:x}, {integer_width}) }} "
            f"else if fs1 >= asfloat(zext(0x{upper:0{source_width // 4}x}, "
            f"{source_width})) {{ zext(0x{maximum:x}, {integer_width}) }} "
            f"else if fs1 < asfloat(zext(0x{lower:0{source_width // 4}x}, "
            f"{source_width})) {{ zext(0x{minimum:x}, {integer_width}) }} "
            "else { value }"
        )
        if integer_width == 32:
            result = f"sext({result}, self.XLEN)"
    else:
        result = f"scalar_canonical{destination_width}(value)"
    body = (
        f"let value = {value};\n"
        "FFLAGS::fflags = FFLAGS::fflags | fp_flags(value);\n"
        f"{destination} = {result};"
    )
    if dynamic:
        body = (
            "if FRM::frm ~> 0b100 {\n    trap(2);\n} else {\n"
            + "\n".join("    " + line for line in body.splitlines())
            + "\n}"
        )
    return "\n".join("        " + line for line in body.splitlines())


def generate():
    defs = [
        "//! Generated by utils/scripts/generate_riscv_float.py.\n",
        "isa F64 requires [F, RV64I] {}\n",
    ]
    for width, nan in [(32, "0x7fc00000"), (64, "0x7ff8000000000000")]:
        defs.append(
            f"fn scalar_canonical{width}(value) {{\n"
            "    if value >= value { value } "
            f"else {{ asfloat(zext({nan}, {width})) }}\n"
            "}\n"
        )
    checks = {
        xlen: [
            f"# RUN: tir mc --march=rv{xlen}ifd %s | filecheck %s",
            f"# RUN: tir mc --march=rv{xlen}ifd --filetype=obj-ascii %s "
            "| filecheck --check-prefix=ENC %s",
            "",
            ".global rounded",
            "rounded:",
        ]
        for xlen in [32, 64]
    }
    for (
        name,
        mnemonic,
        extension,
        operands,
        encoding,
        expression,
        destination,
        encode,
    ) in families():
        destination_width = int(operands[0][1][3:]) if destination == "fd" else None
        source_width = (
            int(operands[1][1][3:]) if operands[1][1].startswith("FPR") else None
        )
        encoding = encoding.replace("self.RM", "RM")
        names = ", ".join("{" + name + "}" for name, _ in operands)
        defs.append(
            f"template {name}Rounded for [{extension}] {{\n"
            f'    param MNEMONIC: String = "{mnemonic}";\n'
            "    param OPNAME: String;\n"
            "    param RM: bits<3>;\n"
            "    operands { "
            + ", ".join(f"{operand}: {ty}" for operand, ty in operands)
            + f", }}\n    encoding {{ {encoding} }}\n}}\n"
        )
        for suffix, rm, text in MODES:
            trailing = f", {text}" if text else ""
            opname = mnemonic + ("." + text if text else "")
            assembly = f'"{{self.MNEMONIC}} {names}{trailing}"'
            if rm == 7:
                assembly = f'({assembly}, "{{self.MNEMONIC}} {names}, dyn")'
            instruction = (
                f"instruction {name}{suffix} for [{extension}] : {name}Rounded {{\n"
                f'    param OPNAME: String = "{opname}";\n'
                f"    param RM: bits<3> = 0b{rm:03b};\n"
                f"    asm {{ {assembly} }}\n"
            )
            instruction_body = behavior(
                expression, destination, rm, source_width, destination_width, mnemonic
            )
            instruction += f"    behavior {{\n{instruction_body}\n    }}\n"
            defs.append(instruction + "}\n")
            args = ", ".join(
                ("f" if ty.startswith("FPR") else "x") + str(10 + i)
                for i, (_, ty) in enumerate(operands)
            )
            canonical = f"{mnemonic} {args}{trailing}"
            spellings = [canonical, canonical + ", dyn"] if rm == 7 else [canonical]
            for line in spellings:
                for xlen in [32, 64]:
                    if xlen == 32 and extension.endswith("64"):
                        continue
                    checks[xlen].append(line)
                    checks[xlen].append(
                        f"# CHECK: {canonical}" + ("{{(?m)$}}" if rm == 7 else "")
                    )
                    checks[xlen].append(
                        "# ENC: ["
                        + ", ".join(
                            f"0x{byte:02X}" for byte in encode(rm).to_bytes(4, "little")
                        )
                        + "]"
                    )
    return {
        ROOT / "backends/riscv/defs/float_round.tmdl": "\n".join(defs),
        **{
            ROOT / f"backends/riscv/checks/asm/float-round-rv{xlen}.S": "\n".join(lines)
            + "\n"
            for xlen, lines in checks.items()
        },
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    for path, content in generate().items():
        if args.check:
            if not path.exists() or path.read_text() != content:
                raise SystemExit(f"out of date: {path.relative_to(ROOT)}")
        else:
            path.write_text(content)
