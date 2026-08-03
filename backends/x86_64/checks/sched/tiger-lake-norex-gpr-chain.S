# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

# Same for the REX-free eax..edi encodings: the destination is read as well as
# written, so the adds serialize at latency 1.

.global f
f:
    add eax, ebx

# CHECK: Total Cycles:      101
