# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  ret
.global _start
_start:
  mov rax, -1
  mov al, 128
  cbw
  mov r8, rax

  mov rax, 0x1000
  sahf
  lahf
  mov r9, rax

  mov rax, 8
  mov rbx, 3
  bt rax, rbx
  mov r10, 0
  setb r10b

  mov rax, 5
  mov rbx, 3
  mov rcx, 7
  cmpxchg rbx, rcx
  mov r11, 0
  setb r11b

  mov rax, 1
  shl rax, 62
  mov rbx, 0
  shld rax, rbx, 1
  mov r12, 0
  seto r12b

  mov rax, -1
  shl rax, 63
  mov rbx, 0
  shrd rax, rbx, 1
  mov r13, 0
  seto r13b

  mov rax, -1
  shl rax, 63
  stc
  rcl rax, 3
  mov r14, rax

  mov rax, 1
  stc
  rcr rax, 3
  mov r15, rax

# CHECK: final registers:
# CHECK-DAG: GPR[8] = 0xffffffffffffff80
# CHECK-DAG: GPR[9] = 0x1200
# CHECK-DAG: GPR[10] = 0x1
# CHECK-DAG: GPR[11] = 0x1
# CHECK-DAG: GPR[12] = 0x1
# CHECK-DAG: GPR[13] = 0x1
# CHECK-DAG: GPR[14] = 0x6
# CHECK-DAG: GPR[15] = 0x6000000000000000
