# Zbs roundtrip and golden encodings per the Sail RISC-V model.
# RUN: tir mc --march=rv32i_zbs %s | filecheck %s
# RUN: tir mc --march=rv64i_zbs %s | filecheck %s
# RUN: tir mc --march=rv32i --mattr=+zbs %s | filecheck %s
# RUN: tir mc --march=rv64i --mattr=+zbs %s | filecheck %s
# RUN: tir mc --march=rv64i_zbs --filetype=obj-ascii %s | filecheck --check-prefix=ENC %s

.global f
f:
    bset x10, x11, x12
    bclr x10, x11, x12
    binv x10, x11, x12
    bext x10, x11, x12
    bseti x10, x11, 5
    bclri x10, x11, 5
    binvi x10, x11, 5
    bexti x10, x11, 5

# CHECK: bset x10, x11, x12
# CHECK-NEXT: bclr x10, x11, x12
# CHECK-NEXT: binv x10, x11, x12
# CHECK-NEXT: bext x10, x11, x12
# CHECK-NEXT: bseti x10, x11, 5
# CHECK-NEXT: bclri x10, x11, 5
# CHECK-NEXT: binvi x10, x11, 5
# CHECK-NEXT: bexti x10, x11, 5
# ENC: [0x33, 0x95, 0xC5, 0x28]
# ENC-NEXT: [0x33, 0x95, 0xC5, 0x48]
# ENC-NEXT: [0x33, 0x95, 0xC5, 0x68]
# ENC-NEXT: [0x33, 0xD5, 0xC5, 0x48]
# ENC-NEXT: [0x13, 0x95, 0x55, 0x28]
# ENC-NEXT: [0x13, 0x95, 0x55, 0x48]
# ENC-NEXT: [0x13, 0x95, 0x55, 0x68]
# ENC-NEXT: [0x13, 0xD5, 0x55, 0x48]
