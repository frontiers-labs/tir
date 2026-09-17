# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  ret
.global _start
_start:
  mov rax, 1
  shl rax, 63
  mov rbx, 0
  shld rax, rbx, 1
  mov r8, 0
  setb r8b

# CHECK: final registers:
# CHECK-DAG: GPR[8] = 0x1
