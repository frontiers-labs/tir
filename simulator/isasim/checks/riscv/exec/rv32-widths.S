# RUN: isasim --march=rv32im --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64im --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

# XLEN comes from the selected ISA: on rv32 registers are 32 bits wide, so
# lui produces a plain 0x80000000, doubling it wraps to zero, -1 is the
# 32-bit all-ones, and a logical right shift only has 32 bits to move.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui  x1, 524288
  add  x3, x1, x1
  addi x4, x0, -1
  srli x9, x4, 28
  mul  x5, x4, x4
  mulh x6, x4, x4

# CHECK: final registers:
# RV32-DAG: GPR[1] = 0x80000000 (width=32)
# RV32-DAG: GPR[3] = 0x0 (width=32)
# RV32-DAG: GPR[4] = 0xffffffff (width=32)
# RV32-DAG: GPR[9] = 0xf (width=32)
# RV64-DAG: GPR[1] = 0xffffffff80000000 (width=64)
# RV64-DAG: GPR[3] = 0xffffffff00000000 (width=64)
# RV64-DAG: GPR[4] = 0xffffffffffffffff (width=64)
# RV64-DAG: GPR[9] = 0xfffffffff (width=64)
# (-1)^2 = 1 with a zero high half on both widths.
# CHECK-DAG: GPR[5] = 0x1
# CHECK-DAG: GPR[6] = 0x0
