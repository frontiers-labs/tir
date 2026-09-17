# RUN: not tir mc --march=rv32i_zba %s
# RUN: not tir mc --march=rv64i_zba %s

.global f
f:
    slli.uw x10, x11, 64
