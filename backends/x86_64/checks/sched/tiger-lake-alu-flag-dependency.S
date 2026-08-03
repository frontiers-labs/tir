# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=1 %s | filecheck %s

# The modeled ALU forms write the flags their behavior computes, so an ALU op
# followed by a flag consumer serializes exactly like `cmp` does. Neither
# instruction writes a register the other reads: only the flag edge orders them.

.global f
f:
    sub rax, rbx
    cmove rcx, rdx

# CHECK: Total Cycles:      3
