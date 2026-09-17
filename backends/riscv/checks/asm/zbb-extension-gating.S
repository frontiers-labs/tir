# Zba and Zbb extension gating: mnemonics must not be accepted without the ISA.
# RUN: not tir mc --march=rv32i %s
# RUN: not tir mc --march=rv64i %s

.global f
f:
    sh1add x10, x11, x12
    andn x10, x11, x12
