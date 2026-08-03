# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    lea rax, [rip + 16]
    lea r9, [r8 + r8]
    lea r10, [rsi + 4*r10]
    lea r14, [rcx + 8]
    lea rdi, [rbp - 60]
    lea rax, [rax + 2*rax]
    lea rdi, [rsi + 8*rdi]
    lea rbx, [4*r9]
    lea rdi, [8*rax]
    lea rax, [r12 - 2]
    lea rdi, [rbp - 192]
    lea rsi, [rsp + 128]
    lea r11, [rsi + rcx + 2]
    lea rdi, [rdi + 2*rdx + 2]
    lea rcx, [r15 + 4*rdx + 12]
    lea rax, [rcx + 8*rax - 16]
    lea rdx, [4*rax + 4]
    lea rsi, [8*rax + 16]
    cmp r9b, 5
    test dil, 4
    cmp al, 35
    test al, 3
    sar r11
    sar eax
    shr rax
    shl rax
    ror eax
    ror rax
    lea rcx, [r13 + rdx]
    lea r8, [r13 + 4*r8]
    lea rsi, [4*r12]

# CHECK: Total Cycles:      969
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      lea rax, [rip + 16]
# CHECK-NEXT: 1      1      lea r9, [r8 + r8]
# CHECK-NEXT: 1      1      lea r10, [rsi + 4*r10]
# CHECK-NEXT: 1      1      lea r14, [rcx + 8]
# CHECK-NEXT: 1      1      lea rdi, [rbp + -60]
# CHECK-NEXT: 1      1      lea rax, [rax + 2*rax]
# CHECK-NEXT: 1      1      lea rdi, [rsi + 8*rdi]
# CHECK-NEXT: 1      1      lea rbx, [4*r9]
# CHECK-NEXT: 1      1      lea rdi, [8*rax]
# CHECK-NEXT: 1      1      lea rax, [r12 + -2]
# CHECK-NEXT: 1      1      lea rdi, [rbp + -192]
# CHECK-NEXT: 1      1      lea rsi, [rsp + 128]
# CHECK-NEXT: 1      1      lea r11, [rsi + rcx + 2]
# CHECK-NEXT: 1      1      lea rdi, [rdi + 2*rdx + 2]
# CHECK-NEXT: 1      1      lea rcx, [r15 + 4*rdx + 12]
# CHECK-NEXT: 1      1      lea rax, [rcx + 8*rax + -16]
# CHECK-NEXT: 1      1      lea rdx, [4*rax + 4]
# CHECK-NEXT: 1      1      lea rsi, [8*rax + 16]
# CHECK-NEXT: 1      1      cmp r9b, 5
# CHECK-NEXT: 1      1      test dil, 4
# CHECK-NEXT: 1      1      cmp al, 35
# CHECK-NEXT: 1      1      test al, 3
# CHECK-NEXT: 1      1      sar r11
# CHECK-NEXT: 1      1      sar eax
# CHECK-NEXT: 1      1      shr rax
# CHECK-NEXT: 1      1      shl rax
# CHECK-NEXT: 1      1      ror eax
# CHECK-NEXT: 1      1      ror rax
# CHECK-NEXT: 1      1      lea rcx, [r13 + rdx]
# CHECK-NEXT: 1      1      lea r8, [r13 + 4*r8]
# CHECK-NEXT: 1      1      lea rsi, [4*r12]
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: -     -     -     1.00  lea rax, [rip + 16]
# CHECK-NEXT: 0.67  0.33  -     -     lea r9, [r8 + r8]
# CHECK-NEXT: -     -     1.00  -     lea r10, [rsi + 4*r10]
# CHECK-NEXT: 0.01  0.66  0.33  -     lea r14, [rcx + 8]
# CHECK-NEXT: 0.33  0.66  0.01  -     lea rdi, [rbp + -60]
# CHECK-NEXT: 0.66  0.34  -     -     lea rax, [rax + 2*rax]
# CHECK-NEXT: 0.34  -     0.66  -     lea rdi, [rsi + 8*rdi]
# CHECK-NEXT: -     -     0.01  0.99  lea rbx, [4*r9]
# CHECK-NEXT: 0.66  0.34  -     -     lea rdi, [8*rax]
# CHECK-NEXT: 0.01  0.33  0.66  -     lea rax, [r12 + -2]
# CHECK-NEXT: -     0.34  -     0.66  lea rdi, [rbp + -192]
# CHECK-NEXT: 0.33  0.33  0.34  -     lea rsi, [rsp + 128]
# CHECK-NEXT: 1.00  -     -     -     lea r11, [rsi + rcx + 2]
# CHECK-NEXT: -     1.00  -     -     lea rdi, [rdi + 2*rdx + 2]
# CHECK-NEXT: 0.67  0.33  -     -     lea rcx, [r15 + 4*rdx + 12]
# CHECK-NEXT: 1.00  -     -     -     lea rax, [rcx + 8*rax + -16]
# CHECK-NEXT: 1.00  -     -     -     lea rdx, [4*rax + 4]
# CHECK-NEXT: -     1.00  -     -     lea rsi, [8*rax + 16]
# CHECK-NEXT: -     1.00  -     -     cmp r9b, 5
# CHECK-NEXT: -     0.33  0.67  -     test dil, 4
# CHECK-NEXT: -     -     1.00  -     cmp al, 35
# CHECK-NEXT: 1.00  -     -     -     test al, 3
# CHECK-NEXT: 1.00  -     -     -     sar r11
# CHECK-NEXT: -     -     1.00  -     sar eax
# CHECK-NEXT: -     1.00  -     -     shr rax
# CHECK-NEXT: -     -     -     1.00  shl rax
# CHECK-NEXT: -     0.67  -     0.33  ror eax
# CHECK-NEXT: -     0.33  0.67  -     ror rax
# CHECK-NEXT: -     -     0.33  0.67  lea rcx, [r13 + rdx]
# CHECK-NEXT: 0.67  -     -     0.33  lea r8, [r13 + 4*r8]
# CHECK-NEXT: 0.33  -     0.67  -     lea rsi, [4*r12]
