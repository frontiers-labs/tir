# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    shl rax, 1

# CHECK: Resource pressure by instruction:
# CHECK: 1.00{{.*}}shl rax, 1
