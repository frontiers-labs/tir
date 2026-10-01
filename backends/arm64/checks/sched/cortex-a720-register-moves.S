# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=200 %s | filecheck %s

# Register-copy throughput remains four instructions per cycle.
.global f
f:
    orr x8, xzr, x26
    orr x1, xzr, x22
    orr x0, xzr, x4

# CHECK: Instructions:      600
# CHECK-NEXT: Decoded uops:      600
# CHECK-NEXT: Execution uops:    600
# CHECK: Total Cycles:      151
