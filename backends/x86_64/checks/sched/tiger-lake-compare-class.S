# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    cmp r8, r9
    test r10, r11

# CHECK: Resource pressure by instruction:
# CHECK: 1.00{{.*}}cmp r8, r9
# CHECK: 1.00{{.*}}test r10, r11
