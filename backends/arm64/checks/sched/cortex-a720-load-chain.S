# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=100 %s | filecheck %s

.global f
f:
    ldr x0, [x0, x1]

# CHECK: Total Cycles:      401
