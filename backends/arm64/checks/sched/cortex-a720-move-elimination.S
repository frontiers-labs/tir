# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=200 %s | filecheck %s

# Constant materialization retains the five-wide front-end charge.
.global f
f:
    movz x21, 3075
    movz w4, 12
    movz w3, 3

# CHECK: Instructions:      600
# CHECK-NEXT: Decoded uops:      600
# CHECK-NEXT: Execution uops:    0
# CHECK: Total Cycles:      121
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 0      1      movz x21, 3075
# CHECK-NEXT: 0      1      movz w4, 12
# CHECK-NEXT: 0      1      movz w3, 3
