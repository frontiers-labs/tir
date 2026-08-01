# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    add rax, rbx

# CHECK: Dispatch Width:    6
