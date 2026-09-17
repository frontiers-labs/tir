# RUN: not tir mc --march=rv32i_zbb %s
# RUN: tir mc --march=rv64i_zbb %s | filecheck %s

.global f
f:
    rolw x10, x11, x12

# CHECK: rolw x10, x11, x12
