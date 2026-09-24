# RUN: tir sched --march=rv64ic --mcpu=spacemit-x100 --iterations=1 %s | filecheck %s

# Equal amounts qualify, but the consumer still waits two cycles for the pair.
.global f
f:
    slli a1, a0, 5
    c.srli a1, 5
    add a2, a1, a3

# CHECK: Instructions:      3
# CHECK: Decoded uops:      3
# CHECK: Execution uops:    2
# CHECK: Fused groups:      1
# CHECK: Total Cycles:      4
