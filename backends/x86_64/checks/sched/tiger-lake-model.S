# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    add rax, rbx

# CHECK: Total Cycles:      101
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      add rax, rbx
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: 0.34  0.33  0.33  -     add rax, rbx
