# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Sub-register views of the integer file: a 16-bit write leaves bits 63:16 of
# the register alone, the two byte views leave each other and the upper 48
# bits alone, a 32-bit write zero-extends across the whole register, and a
# byte write into a fresh register reads back through the full view.
.global done
done:
  add rax, rax
.global _start
_start:
  movabs rax, -1
  mov ax, 0x1234
  movabs rbx, -1
  mov bl, 0x11
  mov bh, 0x22
  movabs rcx, -1
  mov ecx, 0x7eadbeef
  mov dl, 0x7f

# CHECK: final registers:
# CHECK-DAG: GPR[0] = 0xffffffffffff1234
# CHECK-DAG: GPR[3] = 0xffffffffffff2211
# CHECK-DAG: GPR[1] = 0x7eadbeef
# CHECK-DAG: GPR[2] = 0x7f
