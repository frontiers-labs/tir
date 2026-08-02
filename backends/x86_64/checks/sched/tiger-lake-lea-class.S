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

# CHECK: Total Cycles:      2101
# CHECK: [13]: ILea
# CHECK: cmp r9b, 5
# CHECK: test dil, 4
# CHECK: cmp al, 35
# CHECK: test al, 3
# CHECK: sar r11
# CHECK: sar eax
# CHECK: shr rax
# CHECK: shl rax
# CHECK: ror eax
# CHECK: ror rax
# CHECK: lea rcx, [r13 + rdx]
# CHECK: lea r8, [r13 + 4*r8]
# CHECK: lea rsi, [4*r12]
