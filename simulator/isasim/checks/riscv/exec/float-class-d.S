# RUN: isasim --march=rv64ifd --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 1048320
  addi x1, x1, 0
  slli x1, x1, 32
  fmv.d.x f0, x1
  fclass.d x5, f0
  lui x1, 524544
  addi x1, x1, 0
  slli x1, x1, 32
  fmv.d.x f0, x1
  fclass.d x6, f0
  lui x1, 524288
  addi x1, x1, 0
  slli x1, x1, 32
  addi x1, x1, 1
  fmv.d.x f0, x1
  fclass.d x7, f0
  lui x1, 524288
  addi x1, x1, 0
  slli x1, x1, 32
  fmv.d.x f0, x1
  fclass.d x8, f0
  lui x1, 0
  addi x1, x1, 0
  slli x1, x1, 32
  fmv.d.x f0, x1
  fclass.d x9, f0
  lui x1, 0
  addi x1, x1, 0
  slli x1, x1, 32
  addi x1, x1, 1
  fmv.d.x f0, x1
  fclass.d x10, f0
  lui x1, 256
  addi x1, x1, 0
  slli x1, x1, 32
  fmv.d.x f0, x1
  fclass.d x11, f0
  lui x1, 524032
  addi x1, x1, 0
  slli x1, x1, 32
  fmv.d.x f0, x1
  fclass.d x12, f0
  lui x1, 524032
  addi x1, x1, 0
  slli x1, x1, 32
  addi x1, x1, 1
  fmv.d.x f0, x1
  fclass.d x13, f0
  lui x1, 524160
  addi x1, x1, 0
  slli x1, x1, 32
  fmv.d.x f0, x1
  fclass.d x14, f0

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
