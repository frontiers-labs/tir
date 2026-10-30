# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=100 %s | filecheck %s

# 600 independent conversions at one per cycle plus the three-cycle latency tail.
.global f
f:
    scvtf d0, x0
    ucvtf d1, x1
    scvtf s2, x2
    ucvtf s3, x3
    fcvt d4, s6
    fcvt s5, d7

# CHECK: Instructions:      600
# CHECK: Total Cycles:      603
