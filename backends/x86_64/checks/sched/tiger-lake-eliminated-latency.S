# RUN: tir sched --march=x86_64 --mcpu=tiger-lake --iterations=1 %s | filecheck %s

.global f
f:
    xor rax, rax
    xor rbx, rcx

# CHECK: Decoded uops:      2
# CHECK-NEXT: Execution uops:    1
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 0      1      xor rax, rax
# CHECK-NEXT: 1      1      xor rbx, rcx
