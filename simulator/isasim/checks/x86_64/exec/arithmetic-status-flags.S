# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  ret
.global _start
_start:
  mov rax, 0x200
  sahf
  mov rax, 0
  mov rbx, 3
  clc
  adc rax, rbx
  mov r8, 0
  setp r8b

  mov rax, 0x60f
  sahf
  inc al
  lahf
  mov r9, rax

  mov rax, 0x200
  sahf
  mov rax, 1
  mov rbx, 4
  mov rcx, 7
  cmpxchg rbx, rcx
  mov r10, 0
  setp r10b

# CHECK: final registers:
# CHECK-DAG: GPR[8] = 0x1
# CHECK-DAG: GPR[9] = 0x1210
# CHECK-DAG: GPR[10] = 0x1
