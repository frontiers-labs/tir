# RUN: tir sched --march=rv64ic --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

# The destination is independent across copies; the pair can use one
# execution resource while retaining both front-end charges.
.global f
f:
    slli a1, a0, 5
    c.srli a1, 7

# CHECK: Instructions:      200
# CHECK: Decoded uops:      200
# CHECK: Execution uops:    100
# CHECK: Fused groups:      100
# CHECK: Total Cycles:      52
