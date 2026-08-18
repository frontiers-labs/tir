# RUN: tir mc --march=rv64ifd --filetype=obj-ascii %s | filecheck %s

# Encoder golden words, byte-for-byte against clang/llvm-mc for riscv64
# (dynamic rounding for the FP ops). Branch and jump encodings, including
# label fixups and patching, are covered by branch-labels.S and reloc.S.

.global f
f:
    add x10, x11, x12
    addi x5, x6, -1
    jalr x0, 0(x1)
    sd x8, 16(x2)
    ld x8, 16(x2)
    lui x7, 1
    fadd.s fa0, fa0, fa1
    fadd.d fa0, fa0, fa1
    flw fa0, 16(sp)
    fsd fs0, 16(sp)
    fmv.w.x fa0, a0
    fmv.d.x fa0, a0

# CHECK: f:
# add x10, x11, x12 -> 0x00C58533
# CHECK-NEXT: [0x33, 0x85, 0xC5, 0x00]
# addi x5, x6, -1 -> 0xFFF30293
# CHECK-NEXT: [0x93, 0x02, 0xF3, 0xFF]
# jalr x0, x1, 0 (ret) -> 0x00008067
# CHECK-NEXT: [0x67, 0x80, 0x00, 0x00]
# sd x8, 16(x2) -> 0x00813823
# CHECK-NEXT: [0x23, 0x38, 0x81, 0x00]
# ld x8, 16(x2) -> 0x01013403
# CHECK-NEXT: [0x03, 0x34, 0x01, 0x01]
# lui x7, 1 -> 0x000013B7
# CHECK-NEXT: [0xB7, 0x13, 0x00, 0x00]
# fadd.s fa0, fa0, fa1 -> 0x00B57553
# CHECK-NEXT: [0x53, 0x75, 0xB5, 0x00]
# fadd.d fa0, fa0, fa1 -> 0x02B57553
# CHECK-NEXT: [0x53, 0x75, 0xB5, 0x02]
# flw fa0, 16(sp) -> 0x01012507
# CHECK-NEXT: [0x07, 0x25, 0x01, 0x01]
# fsd fs0, 16(sp) -> 0x00813827
# CHECK-NEXT: [0x27, 0x38, 0x81, 0x00]
# fmv.w.x fa0, a0 -> 0xF0050553
# CHECK-NEXT: [0x53, 0x05, 0x05, 0xF0]
# fmv.d.x fa0, a0 -> 0xF2050553
# CHECK-NEXT: [0x53, 0x05, 0x05, 0xF2]
