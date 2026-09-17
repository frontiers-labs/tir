# Zba and Zbb xlen gating: RV64-only and RV32-only encodings on the wrong width.
# RUN: not tir mc --march=rv32i_zba %s
# RUN: tir mc --march=rv64i_zbb --mattr=+zba %s | filecheck %s
# RUN: not tir mc --march=rv32i_zbb %s

.global f
f:
    sh1add.uw x10, x11, x12
    rolw x10, x11, x12
    zext.h x10, x11

# CHECK: sh1add.uw x10, x11, x12
# CHECK-NEXT: rolw x10, x11, x12
# CHECK-NEXT: zext.h x10, x11
