# RUN: isasim --march=rv32i_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# RV32 reads the 64-bit counters as two 32-bit halves: the low half counts
# retired instructions, the high half is zero for short runs.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi  x4, x0, 1
  csrrs x1, instret, x0
  csrrs x2, instreth, x0
  csrrs x3, cycleh, x0

# CHECK: final registers:
# CHECK-DAG: GPR[1] = 0x1
# CHECK-DAG: GPR[2] = 0x0
# CHECK-DAG: GPR[3] = 0x0
