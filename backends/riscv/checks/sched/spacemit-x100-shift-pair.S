# RUN: tir sched --march=rv64ic --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

# Both equal and increasing immediate pairs use the shorter dependent path.
.global equal_amount
 equal_amount:
    c.slli a1, 59
    c.srli a1, 59
.global increasing_amount
 increasing_amount:
    c.slli a2, 59
    c.srli a2, 62

# CHECK: Instructions:      400
# CHECK: Decoded uops:      400
# CHECK: Execution uops:    200
# CHECK: Fused groups:      200
# CHECK: Total Cycles:      101
# CHECK: Resources:
# CHECK-NEXT: [0]: Int0
# CHECK-NEXT: [1]: Int1
# CHECK-NEXT: [2]: Branch
