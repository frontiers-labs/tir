# Zbs shamt values past the 6-bit immediate field are rejected.
# RUN: not tir mc --march=rv32i_zbs %s
# RUN: not tir mc --march=rv64i_zbs %s

.global f
f:
    bseti x10, x11, 64
