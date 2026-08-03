# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=1 %s | filecheck %s

# An 8-bit destination is a merging view of the full register (GPR8's
# `WRITE_POLICY = "merge"`), so `sete` reads rcx as well as writing cl and
# waits for the move that last wrote it. No flag writer appears in the block,
# so the dependency is the merge alone.

.global f
f:
    mov rcx, rdx
    sete cl

# CHECK: Total Cycles:      3
