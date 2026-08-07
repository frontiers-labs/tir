# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-each %s | filecheck %s

# The locked read-modify-write forms write EFLAGS from the value the memory
# location takes. The default test allocation holds `offset & 0xff` in every
# byte, so the dword at 0x80001008 is 0x0b0a0908: `xadd` of its two's complement
# wraps to zero (zf and the carry out set) and hands the pre-add word back in
# ebx, and the following `or` leaves a non-zero, positive word.
.global done
done:
  add rax, rax
.global _start
_start:
  movabs rax, 0x80001000
  mov ebx, 0xf4f5f6f8
  lock xadd [rax + 8], ebx
  lock or [rax + 8], ebx

# CHECK: EFLAGS[0] = 0x1
# CHECK-NEXT: EFLAGS[2] = 0x1
# CHECK-NEXT: EFLAGS[3] = 0x0
# CHECK-NEXT: EFLAGS[4] = 0x0
# CHECK-NEXT: GPR[0] = 0x80001000
# CHECK-NEXT: GPR[3] = 0xb0a0908

# CHECK: EFLAGS[0] = 0x0
# CHECK-NEXT: EFLAGS[2] = 0x0
# CHECK-NEXT: EFLAGS[3] = 0x0
# CHECK-NEXT: EFLAGS[4] = 0x0
