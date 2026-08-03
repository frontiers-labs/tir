# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    movsx rax, bl
    movsxd rcx, edx

# CHECK: Total Cycles:      52
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      movsx rax, bl
# CHECK-NEXT: 1      1      movsxd rcx, edx
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: 0.01  -     0.50  0.49  movsx rax, bl
# CHECK-NEXT: 0.50  0.50  -     -     movsxd rcx, edx
