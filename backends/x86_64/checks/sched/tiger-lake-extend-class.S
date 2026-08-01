# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    movsx rax, bl
    movsxd rcx, edx

# CHECK: Resource pressure by instruction:
# CHECK: 1.00{{.*}}movsx rax, bl
# CHECK: 1.00{{.*}}movsxd rcx, edx
