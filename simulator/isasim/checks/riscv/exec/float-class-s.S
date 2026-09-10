# RUN: isasim --march=rv64ifd --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: isasim --march=rv32if --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 1046528
  addi x1, x1, 0
  fmv.w.x f0, x1
  fclass.s x5, f0
  lui x1, 526336
  addi x1, x1, 0
  fmv.w.x f0, x1
  fclass.s x6, f0
  lui x1, 524288
  addi x1, x1, 1
  fmv.w.x f0, x1
  fclass.s x7, f0
  lui x1, 524288
  addi x1, x1, 0
  fmv.w.x f0, x1
  fclass.s x8, f0
  lui x1, 0
  addi x1, x1, 0
  fmv.w.x f0, x1
  fclass.s x9, f0
  lui x1, 0
  addi x1, x1, 1
  fmv.w.x f0, x1
  fclass.s x10, f0
  lui x1, 2048
  addi x1, x1, 0
  fmv.w.x f0, x1
  fclass.s x11, f0
  lui x1, 522240
  addi x1, x1, 0
  fmv.w.x f0, x1
  fclass.s x12, f0
  lui x1, 522240
  addi x1, x1, 1
  fmv.w.x f0, x1
  fclass.s x13, f0
  lui x1, 523264
  addi x1, x1, 0
  fmv.w.x f0, x1
  fclass.s x14, f0

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x1
# CHECK-DAG: GPR[6] = 0x2
# CHECK-DAG: GPR[7] = 0x4
# CHECK-DAG: GPR[8] = 0x8
# CHECK-DAG: GPR[9] = 0x10
# CHECK-DAG: GPR[10] = 0x20
# CHECK-DAG: GPR[11] = 0x40
# CHECK-DAG: GPR[12] = 0x80
# CHECK-DAG: GPR[13] = 0x100
# CHECK-DAG: GPR[14] = 0x200
