# RUN: isasim --march=rv64ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 523264
  addi x1, x1, 0
  fmv.w.x f0, x1
  fcvt.wu.s x5, f0, rne
  fcvt.l.s x6, f0, rne
  fcvt.lu.s x7, f0, rne
  addi x1, x0, 255
  slli x1, x1, 8
  addi x1, x1, 255
  slli x1, x1, 8
  addi x1, x1, 255
  slli x1, x1, 8
  addi x1, x1, 255
  slli x1, x1, 8
  addi x1, x1, 255
  slli x1, x1, 8
  addi x1, x1, 255
  slli x1, x1, 8
  addi x1, x1, 255
  slli x1, x1, 8
  addi x1, x1, 255
  fcvt.d.lu f0, x1, rne
  fmv.x.d x8, f0
  fcvt.s.lu f0, x1, rne
  fmv.x.w x9, f0
  addi x1, x0, 128
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  fcvt.d.l f0, x1, rne
  fmv.x.d x10, f0
  fcvt.l.d x11, f0, rne
  addi x1, x0, 63
  slli x1, x1, 8
  addi x1, x1, 240
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  addi x1, x1, 16
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  fmv.d.x f0, x1
  fcvt.s.d f1, f0, rne
  fmv.x.w x12, f1
  fcvt.s.d f1, f0, rup
  fmv.x.w x13, f1
  lui x1, 261120
  addi x1, x1, 0
  fmv.w.x f0, x1
  fcvt.d.s f1, f0, rne
  fmv.x.d x14, f1
  lui x1, 0xc0200
  fmv.w.x f0, x1
  fcvt.w.s x15, f0, rne
  fcvt.w.s x16, f0, rtz
  fcvt.w.s x17, f0, rdn
  fcvt.w.s x18, f0, rup
  fcvt.w.s x19, f0, rmm

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0xffffffffffffffff
# CHECK-DAG: GPR[6] = 0x7fffffffffffffff
# CHECK-DAG: GPR[7] = 0xffffffffffffffff
# CHECK-DAG: GPR[8] = 0x43f0000000000000
# CHECK-DAG: GPR[9] = 0x5f800000
# CHECK-DAG: GPR[10] = 0xc3e0000000000000
# CHECK-DAG: GPR[11] = 0x8000000000000000
# CHECK-DAG: GPR[12] = 0x3f800000
# CHECK-DAG: GPR[13] = 0x3f800001
# CHECK-DAG: GPR[14] = 0x3ff8000000000000
# CHECK-DAG: GPR[15] = 0xfffffffffffffffe
# CHECK-DAG: GPR[16] = 0xfffffffffffffffe
# CHECK-DAG: GPR[17] = 0xfffffffffffffffd
# CHECK-DAG: GPR[18] = 0xfffffffffffffffe
# CHECK-DAG: GPR[19] = 0xfffffffffffffffd
