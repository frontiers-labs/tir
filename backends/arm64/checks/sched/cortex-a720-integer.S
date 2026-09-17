# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=100 %s | filecheck %s

.global f
f:
    add x0, x1, x2
    sub x3, x4, x5
    and x6, x7, x8
    lsrv x9, x10, x11

# CHECK: Total Cycles:      101
# CHECK: Resource pressure per iteration:
# CHECK-NEXT: [0]   [1]   [2]   [3]   [4]   [5]   [6]   [7]   [8]   [9]
# CHECK-NEXT: 1.00  1.00  1.00  1.00  0.00  0.00  0.00  0.00  0.00  0.00
