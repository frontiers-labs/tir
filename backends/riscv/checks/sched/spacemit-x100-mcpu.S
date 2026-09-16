# RUN: tir sched --march=rv64i --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s
# RUN: tir sched --march=rv64i --model=spacemit-x100 --iterations=100 %s | filecheck %s

.global f
f:
    add x10, x10, x11

# CHECK: Total Cycles:      101
# CHECK-NEXT: Dispatch Width:    4
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      add x10, x10, x11
