# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=100 %s | filecheck %s

# The two independent load chains must overlap despite each dependent add.
.global f
f:
    ldur x0, [x0, 0]
    add x1, x0, x3
    ldur x2, [x2, 0]
    add x4, x2, x5

# CHECK: Instructions:      400
# CHECK: Total Cycles:      402
