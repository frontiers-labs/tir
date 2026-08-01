# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    movq xmm0, rax
    movq rbx, xmm1

# CHECK: Resource pressure by instruction:
# CHECK: 1.00{{.*}}movq xmm0, rax
# CHECK: 1.00{{.*}}movq rbx, xmm1
