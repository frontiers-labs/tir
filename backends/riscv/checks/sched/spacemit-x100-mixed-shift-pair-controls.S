# RUN: tir sched --march=rv64ic --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

# Larger left amount and a recurrent source are outside the measured pattern.
.global greater_amount
 greater_amount:
    slli a1, a0, 9
    c.srli a1, 5
.global recurrent_source
 recurrent_source:
    slli a1, a1, 5
    c.srli a1, 7

# CHECK: Instructions:      400
# CHECK: Decoded uops:      400
# CHECK: Execution uops:    400
# CHECK: Fused groups:      0
# CHECK: Total Cycles:      135
