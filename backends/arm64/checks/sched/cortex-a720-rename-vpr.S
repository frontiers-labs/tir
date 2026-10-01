# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=10 %s | filecheck %s

# Independent work behind a divide must exceed the architectural rename pool.
.global f
f:
    fdiv d0, d0, d1
    fdiv d0, d0, d1
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    ldr q2, [sp, 0]
    fdiv d5, d5, d6

# CHECK: Instructions:      350
# CHECK: Total Cycles:      261
