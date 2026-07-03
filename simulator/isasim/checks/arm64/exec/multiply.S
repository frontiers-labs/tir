# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Multiply/divide family: 6 * 7 = 42 (0x2a); smulh of -3 * 7 is the high half
# of the signed 128-bit product (-1); madd/msub/mneg accumulate; sdiv truncates
# toward zero, udiv treats -3 as 2^64 - 3, and division by zero yields 0.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  add x1, x0, 6
  add x2, x0, 7
  mul x3, x1, x2
  movn x4, 2
  smulh x5, x4, x2
  madd x6, x1, x2, x1
  msub x7, x1, x2, x1
  mneg x8, x1, x2
  add x9, x0, 100
  sdiv x10, x9, x2
  sdiv x11, x4, x2
  udiv x12, x4, x2
  udiv x13, x9, x0

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0x2a
# CHECK-DAG: GPR[4] = 0xfffffffffffffffd
# CHECK-DAG: GPR[5] = 0xffffffffffffffff
# CHECK-DAG: GPR[6] = 0x30
# CHECK-DAG: GPR[7] = 0xffffffffffffffdc
# CHECK-DAG: GPR[8] = 0xffffffffffffffd6
# CHECK-DAG: GPR[10] = 0xe
# CHECK-DAG: GPR[11] = 0x0
# CHECK-DAG: GPR[12] = 0x2492492492492491
# CHECK-DAG: GPR[13] = 0x0
