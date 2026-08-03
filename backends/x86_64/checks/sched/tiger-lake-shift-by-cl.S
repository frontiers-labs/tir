# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=1 %s | filecheck %s

# The by-cl shift reads the count out of rcx implicitly, so it waits for the
# move that wrote rcx.

.global f
f:
    mov rcx, rdx
    shl rax, cl

# CHECK: Total Cycles:      3
