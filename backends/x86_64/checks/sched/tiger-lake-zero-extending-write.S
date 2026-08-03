# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=1 %s | filecheck %s

# A 32-bit destination zero-extends into the full register instead of merging,
# so the immediate move starts a fresh dependency chain and does not wait for
# the write of rcx before it.

.global f
f:
    mov rcx, rdx
    mov ecx, 1

# CHECK: Total Cycles:      2
