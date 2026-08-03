# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    ucomisd xmm0, xmm1

# CHECK: Total Cycles:      101
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      ucomisd xmm0, xmm1
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: 1.00  -     -     -     ucomisd xmm0, xmm1
