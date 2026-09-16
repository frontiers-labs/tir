# RUN: tir sched --march=rv64i --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

.global f
f:
    slli x10, x11, 2
    srli x12, x13, 2
    srai x14, x15, 2
    slliw x16, x17, 2
    srliw x18, x19, 2
    sraiw x20, x21, 2

# CHECK: Total Cycles:      201
