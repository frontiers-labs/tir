# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=1 %s | filecheck %s

# `cqo` reads rax and writes rdx by path, so it sits between the move that sets
# up the dividend and the move that consumes the sign extension.

.global f
f:
    mov rax, rbx
    cqo
    mov rcx, rdx

# CHECK: Total Cycles:      4
