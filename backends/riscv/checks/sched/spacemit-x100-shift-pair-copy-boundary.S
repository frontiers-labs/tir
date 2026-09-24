# RUN: tir sched --march=rv64ic --mcpu=spacemit-x100 --iterations=1 %s | filecheck %s

# Each copy starts right-left. The middle left-right pair spans the copies.
.global f
f:
    c.srli a1, 32
    c.slli a1, 32
    c.srli a1, 32
    c.slli a1, 32

# CHECK: Instructions:      4
# CHECK: Decoded uops:      4
# CHECK: Execution uops:    3
# CHECK: Fused groups:      1
