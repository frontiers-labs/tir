# RUN: isasim --march=armv8-a --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  movz x1, 32768
  add x2, x0, 16
  lslv x1, x1, x2
  movz x3, 4096
  add x1, x1, x3
  movz x4, 48879
  ldxr x5, [x1]
  stxr w6, x4, [x1]
  stxr w7, x5, [x1]
  ldaxr x8, [x1]
  stlxr w9, x5, [x1]
  ldxr w10, [x1]

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x706050403020100
# CHECK-DAG: GPR[6] = 0x0
# CHECK-DAG: GPR[7] = 0x1
# CHECK-DAG: GPR[8] = 0xbeef
# CHECK-DAG: GPR[9] = 0x0
# CHECK-DAG: GPR[10] = 0x3020100
