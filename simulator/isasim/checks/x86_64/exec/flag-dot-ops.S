# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Flag-dot and stack primitives: stc/adc/cmc/clc drive carry through the
# borrow group, pushf/popf round-trips the flag word, and the frame-pointer
# pair push-rbp/mov/leave restores rsp and rbp exactly.

.global done
done:
  add rax, rax
.global _start
_start:
  movabs rsp, 0x80010000
  stc
  mov eax, 3
  adc rax, 2
  cmc
  mov r8, rax
  clc
  movabs rbx, -1
  sbb rax, rbx
  mov r9, rax
  movabs rcx, 0x7fffffffffffffff
  add rcx, 1
  pushf
  test rcx, rcx
  popf
  mov r10, 0
  seto r10b
  movabs rsp, 0x80010000
  push rax
  mov rbp, rsp
  leave

# CHECK: final registers:
# CHECK-DAG: GPR[0] = 0x7
# CHECK-DAG: GPR[3] = 0xffffffffffffffff
# CHECK-DAG: GPR[8] = 0x6
# CHECK-DAG: GPR[9] = 0x7
# CHECK-DAG: GPR[10] = 0x1
