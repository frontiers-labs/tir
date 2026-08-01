# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    imul rax, rbx

# CHECK:      [1]    [2]    Instructions:
# CHECK-NEXT: 3      1      imul rax, rbx
