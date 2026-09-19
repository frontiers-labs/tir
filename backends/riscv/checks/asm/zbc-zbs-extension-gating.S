# Zbc and Zbs extension gating: mnemonics must not be accepted without the ISA.
# RUN: not tir mc --march=rv32i %s
# RUN: not tir mc --march=rv64i %s
# RUN: tir mc --march=rv32i --mattr=+zbc,+zbs %s | filecheck %s
# RUN: tir mc --march=rv64i --mattr=+zbc,+zbs %s | filecheck %s
# RUN: not tir mc --march=rv32i_zbc_zbs --mattr=-zbc,-zbs %s
# RUN: not tir mc --march=rv64i_zbc_zbs --mattr=-zbc,-zbs %s

.global f
f:
    clmul x10, x11, x12
    bset x10, x11, x12

# CHECK: clmul x10, x11, x12
# CHECK-NEXT: bset x10, x11, x12
