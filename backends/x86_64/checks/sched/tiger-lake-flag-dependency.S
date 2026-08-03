# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=1 %s | filecheck %s

# `cmp` writes the flags its behavior assigns and `cmovcc` reads the ones its
# guard tests, so the pair serializes. A single iteration isolates the flag
# dependency: neither instruction writes a register the other reads.

.global f
f:
    cmp rax, rbx
    cmove rcx, rdx

# CHECK: Total Cycles:      3
