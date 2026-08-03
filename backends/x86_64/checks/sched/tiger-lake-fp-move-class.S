# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    movq xmm0, rax
    movq rbx, xmm1

# CHECK: Total Cycles:      64
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      movq xmm0, rax
# CHECK-NEXT: 1      1      movq rbx, xmm1
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: 0.50  -     0.50  -     movq xmm0, rax
# CHECK-NEXT: -     0.50  -     0.50  movq rbx, xmm1
