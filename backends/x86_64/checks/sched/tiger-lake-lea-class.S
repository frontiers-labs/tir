# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    lea rax, [rip + 16]

# CHECK: Total Cycles:      101
# CHECK: [13]: ILea
