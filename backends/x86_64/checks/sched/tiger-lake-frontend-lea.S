# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    lea rax, [r8 + r9]
    lea rbx, [r10 + r11]
    lea r12, [r13 + r14]
    lea r15, [rsi + rdi]

# Four independent four-byte simple LEAs saturate both the measured four-wide
# decoder and sixteen-byte fetch path.
# CHECK: Total Cycles:      101
# CHECK: [{{[0-9]+}}]: ILeaAny
