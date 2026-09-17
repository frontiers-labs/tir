# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=100 %s | filecheck %s

.global f
f:
    ldr x0, [x20, x21]
    ldr x1, [x20, x21]
    ldr x2, [x20, x21]
    ldr x3, [x20, x21]

# CHECK: Total Cycles:      204
