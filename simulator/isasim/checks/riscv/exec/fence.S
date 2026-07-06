# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# FENCE and FENCE.I are architectural no-ops on a single hart: the program runs
# to completion and the surrounding arithmetic is unaffected (5 + 7 = 12).
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi  t0, x0, 5
  fence 3, 3
  addi  t1, x0, 7
  fence.i
  add   t2, t0, t1

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x5
# CHECK-DAG: GPR[6] = 0x7
# CHECK-DAG: GPR[7] = 0xc
