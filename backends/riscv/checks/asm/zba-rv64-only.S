# RUN: not tir mc --march=rv32i_zba %s
# RUN: tir mc --march=rv64i_zba %s | filecheck %s

.global f
f:
    sh1add.uw x10, x11, x12

# CHECK: sh1add.uw x10, x11, x12
