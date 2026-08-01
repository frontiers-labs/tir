# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    mov rbx, 7
    movabs rcx, 4294967296

# CHECK: [8]: IMoveImmediate
# CHECK: [9]: IMoveAbsolute
# CHECK: Resource pressure by instruction:
# CHECK: 1.00{{.*}}mov rbx, 7
# CHECK: 1.00{{.*}}movabs rcx, 4294967296
