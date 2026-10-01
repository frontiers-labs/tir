# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=100 %s | filecheck %s

# Five stores per iteration share the two-wide store resource, including W-register aliases.
.global f
f:
    str w0, [sp, 0]
    str d1, [sp, 8]
    stur w2, [sp, -4]
    strb w3, [sp, 12]
    strh w4, [sp, 16]

# CHECK: Instructions:      500
# CHECK: Total Cycles:      252
