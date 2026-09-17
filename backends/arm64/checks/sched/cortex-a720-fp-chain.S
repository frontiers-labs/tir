# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=100 %s | filecheck %s

.global f
f:
    fmul s0, s0, s1

# CHECK: Total Cycles:      301
