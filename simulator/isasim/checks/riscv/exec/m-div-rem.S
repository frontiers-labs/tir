# RUN: isasim --march=rv64im --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: not isasim --march=rv64i --entry=_start --until-pc=done %s

# Full RV64M divide/remainder and high-multiply execution, including the
# architectural corner cases: division by zero, and signed *W division of a
# negative dividend (which must sign-interpret the low 32 bits).
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x1, x0, -1            # 0xffffffffffffffff
  addi x2, x0, 2
  addi x6, x0, -7
  addi x13, x0, 7
  addi x15, x0, 3
  mulhu  x3, x1, x2          # high64((2^64-1)*2) = 1
  mulhsu x4, x1, x2          # high64(signed -1 * unsigned 2) = -1
  div    x7, x6, x2          # -7 / 2 = -3
  div    x8, x6, x0          # divide by zero -> -1
  divu   x9, x1, x2          # (2^64-1) / 2
  divu   x10, x2, x0         # unsigned divide by zero -> all ones
  rem    x11, x6, x2         # -7 % 2 = -1
  rem    x12, x6, x0         # remainder by zero -> dividend (-7)
  remu   x14, x13, x15       # 7 % 3 = 1
  remu   x16, x13, x0        # unsigned remainder by zero -> dividend (7)
  mulw   x17, x1, x2         # sext32(low32(-1 * 2)) = -2
  divw   x18, x6, x2         # signed 32-bit -7 / 2 = -3
  divuw  x19, x1, x2         # unsigned 32-bit (2^32-1) / 2 = 0x7fffffff
  remw   x20, x6, x2         # signed 32-bit -7 % 2 = -1
  remuw  x21, x13, x15       # unsigned 32-bit 7 % 3 = 1

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0x1
# CHECK-DAG: GPR[4] = 0xffffffffffffffff
# CHECK-DAG: GPR[7] = 0xfffffffffffffffd
# CHECK-DAG: GPR[8] = 0xffffffffffffffff
# CHECK-DAG: GPR[9] = 0x7fffffffffffffff
# CHECK-DAG: GPR[10] = 0xffffffffffffffff
# CHECK-DAG: GPR[11] = 0xffffffffffffffff
# CHECK-DAG: GPR[12] = 0xfffffffffffffff9
# CHECK-DAG: GPR[14] = 0x1
# CHECK-DAG: GPR[16] = 0x7
# CHECK-DAG: GPR[17] = 0xfffffffffffffffe
# CHECK-DAG: GPR[18] = 0xfffffffffffffffd
# CHECK-DAG: GPR[19] = 0x7fffffff
# CHECK-DAG: GPR[20] = 0xffffffffffffffff
# CHECK-DAG: GPR[21] = 0x1
