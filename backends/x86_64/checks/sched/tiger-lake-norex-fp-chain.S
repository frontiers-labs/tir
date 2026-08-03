# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

# The REX-free encoding the assembler picks for xmm0..xmm7 is two-address, so a
# repeated mulsd is a loop-carried chain of latency-4 multiplies.

.global f
f:
    mulsd xmm1, xmm2

# CHECK: Total Cycles:      401
