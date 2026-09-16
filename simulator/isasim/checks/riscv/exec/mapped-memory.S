# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --engine=interp --mem-size=0 --memory-config=%S/Inputs/mapped-memory.json --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x10, 0x100
  lw x2, 0(x10)

# CHECK: GPR[2] = 0x12345678
