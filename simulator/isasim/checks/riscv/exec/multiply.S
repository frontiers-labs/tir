# RUN: isasim --march=rv64im --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: not isasim --march=rv64i --entry=_start --until-pc=done %s

# M extension: 6 * 7 = 42 (0x2a); mulh of -3 * 7 is the high half of the
# signed 128-bit product, i.e. -1.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x1, x0, 6
  addi x2, x0, 7
  mul  x3, x1, x2
  addi x4, x0, -3
  mulh x5, x4, x2

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0x2a
# CHECK-DAG: GPR[5] = 0xffffffffffffffff
