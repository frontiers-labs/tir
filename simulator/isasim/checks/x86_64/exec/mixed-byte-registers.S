# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add rax, rax
.global _start
_start:
  movabs rax, 0x22
  mov bh, al
  mov bl, bh
  add bh, al
  sub al, bh
  mov ah, bl
  or dl, ah
  xor ch, al
  mov cl, ah
  sub ah, cl
  add dl, ch

# CHECK: final registers:
# CHECK-DAG: GPR[0] = 0xde
# CHECK-DAG: GPR[1] = 0xde22
# CHECK-DAG: GPR[2] = 0x0
# CHECK-DAG: GPR[3] = 0x4422
