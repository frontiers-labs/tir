# RUN: isasim --march=rv64i_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Every Zicsr form returns the pre-write CSR value in rd, then applies its
# modification: write, set bits, clear bits, write immediate.
.global done
done:
  add x0, x0, x0
.global first
first:
  addi  x1, x0, 3          # 0b0011
  addi  x4, x0, 4          # 0b0100
  addi  x6, x0, 2          # 0b0010
  csrrw x2, mscratch, x1   # x2 = 0,      mscratch = 0b0011
  csrrs x3, mscratch, x4   # x3 = 0b0011, mscratch = 0b0111
  csrrc x5, mscratch, x6   # x5 = 0b0111, mscratch = 0b0101
  csrrwi x7, mscratch, 9   # x7 = 0b0101, mscratch = 9
  csrrs x8, mscratch, x0   # x8 = 9 (read back)
.global _start
_start:
  add x0, x0, x0

# CHECK: final registers:
# CHECK-DAG: GPR[2] = 0x0
# CHECK-DAG: GPR[3] = 0x3
# CHECK-DAG: GPR[5] = 0x7
# CHECK-DAG: GPR[7] = 0x5
# CHECK-DAG: GPR[8] = 0x9
