# Zbs RV64-only shamt values: 32..63 need the 6-bit immediate field.
# RUN: tir mc --march=rv64i_zbs %s | filecheck %s
# RUN: tir mc --march=rv64i_zbs --filetype=obj-ascii %s | filecheck --check-prefix=ENC %s

.global f
f:
    bseti x10, x11, 32
    bclri x10, x11, 32
    binvi x10, x11, 32
    bexti x10, x11, 32
    bseti x10, x11, 63
    bclri x10, x11, 63
    binvi x10, x11, 63
    bexti x10, x11, 63

# CHECK: bseti x10, x11, 32
# CHECK-NEXT: bclri x10, x11, 32
# CHECK-NEXT: binvi x10, x11, 32
# CHECK-NEXT: bexti x10, x11, 32
# CHECK-NEXT: bseti x10, x11, 63
# CHECK-NEXT: bclri x10, x11, 63
# CHECK-NEXT: binvi x10, x11, 63
# CHECK-NEXT: bexti x10, x11, 63
# ENC: [0x13, 0x95, 0x05, 0x2A]
# ENC-NEXT: [0x13, 0x95, 0x05, 0x4A]
# ENC-NEXT: [0x13, 0x95, 0x05, 0x6A]
# ENC-NEXT: [0x13, 0xD5, 0x05, 0x4A]
# ENC-NEXT: [0x13, 0x95, 0xF5, 0x2B]
# ENC-NEXT: [0x13, 0x95, 0xF5, 0x4B]
# ENC-NEXT: [0x13, 0x95, 0xF5, 0x6B]
# ENC-NEXT: [0x13, 0xD5, 0xF5, 0x4B]
