# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=100 %s | filecheck %s
# RUN: tir sched --march=armv8.0-a --model=cortex-a720 --iterations=100 %s | filecheck %s

.global f
f:
    add x0, x0, x1

# CHECK: Total Cycles:      101
# CHECK-NEXT: Dispatch Width:    5
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      add x0, x0, x1
