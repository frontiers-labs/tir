# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    sete al
    cmove rax, rbx

# CHECK: Total Cycles:      201
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      sete al
# CHECK-NEXT: 1      1      cmove rax, rbx
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: -     -     1.00  -     sete al
# CHECK-NEXT: -     -     -     1.00  cmove rax, rbx
