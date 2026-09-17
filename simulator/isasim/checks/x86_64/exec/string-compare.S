# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-each %s | filecheck %s

.global done
done:
  add rax, rax
.global _start
_start:
  movabs rsi, 0x80001008
  movabs rdi, 0x80001009
  cmpsb

# 0x08 - 0x09 = 0xff, and both pointers advance by one.
# CHECK: EFLAGS[0] = 0x1
# CHECK-NEXT: EFLAGS[1] = 0x1
# CHECK-NEXT: EFLAGS[2] = 0x0
# CHECK-NEXT: EFLAGS[3] = 0x1
# CHECK-NEXT: EFLAGS[4] = 0x0
# CHECK-NEXT: EFLAGS[5] = 0x1
# CHECK-NEXT: GPR[6] = 0x80001009
# CHECK-NEXT: GPR[7] = 0x8000100a
