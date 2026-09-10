# RUN: isasim --march=rv64ifd --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 0x80000
  fmv.w.x f0, x1
  fmv.w.x f1, x0
  fmin.s f2, f0, f1
  fmv.x.w x5, f2
  fmax.s f2, f0, f1
  fmv.x.w x6, f2
  feq.s x7, f0, f1
  flt.s x8, f0, f1
  fle.s x9, f0, f1
  lui x1, 0x7fc00
  addi x1, x1, 42
  fmv.w.x f3, x1
  fmin.s f2, f3, f0
  fmv.x.w x10, f2
  fmax.s f2, f3, f3
  fmv.x.w x11, f2
  fsgnj.s f2, f3, f0
  fmv.x.w x12, f2
  fsgnjn.s f2, f3, f0
  fmv.x.w x13, f2
  fsgnjx.s f2, f0, f0
  fmv.x.w x14, f2
  lui x1, 0x80000
  slli x1, x1, 32
  fmv.d.x f0, x1
  fmv.d.x f1, x0
  fmin.d f2, f0, f1
  fmv.x.d x15, f2
  fmax.d f2, f0, f1
  fmv.x.d x16, f2
  lui x1, 0x7ff80
  slli x1, x1, 32
  addi x1, x1, 42
  fmv.d.x f3, x1
  fmin.d f2, f3, f0
  fmv.x.d x17, f2
  fmax.d f2, f3, f3
  fmv.x.d x18, f2
  fsgnj.d f2, f3, f0
  fmv.x.d x19, f2
  fsgnjn.d f2, f3, f0
  fmv.x.d x20, f2
  fsgnjx.d f2, f0, f0
  fmv.x.d x21, f2

  lui x1, 0x3f800
  fmv.w.x f0, x1
  lui x1, 0x40000
  fmv.w.x f1, x1
  flt.s x22, f0, f1
  fle.s x23, f1, f0
  feq.s x24, f0, f1
  fmin.s f2, f1, f0
  fmv.x.w x25, f2
  fmax.s f2, f0, f1
  fmv.x.w x26, f2
  lui x1, 0x7fc00
  fmv.w.x f3, x1
  feq.s x27, f3, f3
  flt.s x28, f3, f0
  fle.s x29, f0, f3

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0xffffffff80000000
# CHECK-DAG: GPR[6] = 0x0
# CHECK-DAG: GPR[7] = 0x1
# CHECK-DAG: GPR[8] = 0x0
# CHECK-DAG: GPR[9] = 0x1
# CHECK-DAG: GPR[10] = 0xffffffff80000000
# CHECK-DAG: GPR[11] = 0x7fc00000
# CHECK-DAG: GPR[12] = 0xffffffffffc0002a
# CHECK-DAG: GPR[13] = 0x7fc0002a
# CHECK-DAG: GPR[14] = 0x0
# CHECK-DAG: GPR[15] = 0x8000000000000000
# CHECK-DAG: GPR[16] = 0x0
# CHECK-DAG: GPR[17] = 0x8000000000000000
# CHECK-DAG: GPR[18] = 0x7ff8000000000000
# CHECK-DAG: GPR[19] = 0xfff800000000002a
# CHECK-DAG: GPR[20] = 0x7ff800000000002a
# CHECK-DAG: GPR[21] = 0x0
# CHECK-DAG: GPR[22] = 0x1
# CHECK-DAG: GPR[23] = 0x0
# CHECK-DAG: GPR[24] = 0x0
# CHECK-DAG: GPR[25] = 0x3f800000
# CHECK-DAG: GPR[26] = 0x40000000
# CHECK-DAG: GPR[27] = 0x0
# CHECK-DAG: GPR[28] = 0x0
# CHECK-DAG: GPR[29] = 0x0
