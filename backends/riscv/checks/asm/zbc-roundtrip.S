# Zbc roundtrip and golden encodings per the Sail RISC-V model.
# RUN: tir mc --march=rv32i_zbc %s | filecheck %s
# RUN: tir mc --march=rv64i_zbc %s | filecheck %s
# RUN: tir mc --march=rv64i_zbc --filetype=obj-ascii %s | filecheck --check-prefix=ENC %s

.global f
f:
    clmul x10, x11, x12
    clmulh x10, x11, x12
    clmulr x10, x11, x12

# CHECK: clmul x10, x11, x12
# CHECK-NEXT: clmulh x10, x11, x12
# CHECK-NEXT: clmulr x10, x11, x12
# ENC: [0x33, 0x95, 0xC5, 0x0A]
# ENC-NEXT: [0x33, 0xB5, 0xC5, 0x0A]
# ENC-NEXT: [0x33, 0xA5, 0xC5, 0x0A]
