# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    ucomisd xmm0, xmm1

# CHECK: [12]: FPCompare
# CHECK: Resource pressure by instruction:
# CHECK: 1.00{{.*}}ucomisd xmm0, xmm1
