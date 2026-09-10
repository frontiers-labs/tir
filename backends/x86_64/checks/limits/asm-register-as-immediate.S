# RUN: not tir mc --march=x86_64 --stage=isel %s 2>&1 | filecheck %s
# RUN: echo mov r8b, bh | not tir mc --march=x86_64 --stage=isel - assembly 2>&1 | filecheck %s
# RUN: echo mov bh, spl | not tir mc --march=x86_64 --stage=isel - assembly 2>&1 | filecheck %s
# RUN: echo mov spl, bh | not tir mc --march=x86_64 --stage=isel - assembly 2>&1 | filecheck %s
# RUN: echo mov bh, eax | not tir mc --march=x86_64 --stage=isel - assembly 2>&1 | filecheck %s
# RUN: echo mov al, xmm0 | not tir mc --march=x86_64 --stage=isel - assembly 2>&1 | filecheck %s
# RUN: echo add bh, rax | not tir mc --march=x86_64 --stage=isel - assembly 2>&1 | filecheck %s --check-prefix=ADD
# RUN: echo shl bh, al | not tir mc --march=x86_64 --stage=isel - assembly 2>&1 | filecheck %s --check-prefix=SHIFT

.global invalid
invalid:
  mov bh, r8b

# CHECK: invalid operands for instruction 'mov'
# ADD: invalid operands for instruction 'add'
# SHIFT: invalid operands for instruction 'shl'
