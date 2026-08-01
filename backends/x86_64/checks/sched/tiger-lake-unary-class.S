# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    neg r8
    not r9

# CHECK: Resource pressure by instruction:
# CHECK: 1.00{{.*}}neg r8
# CHECK: 1.00{{.*}}not r9
