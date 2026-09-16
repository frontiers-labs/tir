# RUN: tir sched --march=rv64i --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

.global f
f:
    auipc x10, 1
    auipc x11, 2
    auipc x12, 3

# CHECK: Total Cycles:      301
