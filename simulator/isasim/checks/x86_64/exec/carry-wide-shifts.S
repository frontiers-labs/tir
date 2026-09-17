# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# The carry/borrow and double-precision-shift group: shrd/shld move bits
# across a register pair, adc adds with an explicit carry-in, and mul widens
# through an unsigned product.

.global done
done:
  add rax, rax
.global _start
_start:
  movabs rax, -2401053092612145147
  movabs rcx, -1152921504606846976
  shrd rax, rcx, 7
  mov r8, rax
  shld rax, rcx, 5
  mov r10, rax
  movabs rax, -2401053092612145147
  movabs rcx, 2
  mul rcx
  mov r9, rax
  stc
  adc rax, rcx

# CHECK: final registers:
# CHECK-DAG: GPR[0] = 0xbd5b7dde0000000d
# CHECK-DAG: GPR[2] = 0x1
# CHECK-DAG: GPR[8] = 0x1bd5b7dde000000
# CHECK-DAG: GPR[9] = 0xbd5b7dde0000000a
# CHECK-DAG: GPR[10] = 0x37ab6fbbc000001e
