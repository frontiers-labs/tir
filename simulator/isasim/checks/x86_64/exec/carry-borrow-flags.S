# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  ret
.global _start
_start:
  mov rax, 0
  mov rbx, -1
  stc
  adc rax, rbx
  mov r8, 0
  setb r8b
  mov r9, 0
  seto r9b

  mov rcx, 0
  stc
  sbb rcx, rbx
  mov r10, 0
  setb r10b
  mov r11, 0
  seto r11b

# CHECK: final registers:
# CHECK-DAG: GPR[0] = 0x0
# CHECK-DAG: GPR[1] = 0x0
# CHECK-DAG: GPR[8] = 0x1
# CHECK-DAG: GPR[9] = 0x0
# CHECK-DAG: GPR[10] = 0x1
# CHECK-DAG: GPR[11] = 0x0
