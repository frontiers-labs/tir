# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

# The widening multiply reads rax and writes rdx:rax implicitly. That closes the
# loop-carried chain r8 -> rax -> (imul, latency 3) -> rdx -> r8.

.global f
f:
    mov rax, r8
    imul rsi
    mov r8, rdx

# CHECK: Total Cycles:      501
