# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    mov rbx, 7
    movabs rcx, 4294967296

# CHECK: Total Cycles:      108
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      mov rbx, 7
# CHECK-NEXT: 1      1      movabs rcx, 4294967296
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: 0.50  -     0.50  -     mov rbx, 7
# CHECK-NEXT: -     0.50  -     0.50  movabs rcx, 4294967296
