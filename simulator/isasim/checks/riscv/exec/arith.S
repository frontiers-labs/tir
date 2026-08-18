# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Execution correctness: 5 + 7 = 12 (0xc); a write to x0 is discarded.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x1, x0, 5
  addi x2, x0, 7
  add  x3, x1, x2
  add  x0, x1, x2

# CHECK: final registers:
# CHECK-NOT: GPR[0]
# CHECK-DAG: GPR[1] = 0x5
# CHECK-DAG: GPR[2] = 0x7
# CHECK-DAG: GPR[3] = 0xc
