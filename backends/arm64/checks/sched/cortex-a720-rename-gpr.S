# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=10 %s | filecheck %s

# Independent work behind a divide must exceed the architectural rename pool.
.global f
f:
    udiv x0, x0, x1
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    add x2, x3, x4
    udiv x5, x5, x6

# CHECK: Instructions:      340
# CHECK: Total Cycles:      209
