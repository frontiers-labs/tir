# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    mov rax, rbx
    mov al, bl

# CHECK:      [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      mov rax, rbx
# CHECK-NEXT: 1      1      mov al, bl
