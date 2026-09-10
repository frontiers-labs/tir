# RUN: isasim --march=rv64ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 260096
  addi x1, x1, 0
  fmv.w.x f0, x1
  lui x1, 210944
  addi x1, x1, 0
  fmv.w.x f1, x1
  fadd.s f2, f0, f1, rne
  fmv.x.w x5, f2
  fadd.s f2, f0, f1, rtz
  fmv.x.w x6, f2
  fadd.s f2, f0, f1, rdn
  fmv.x.w x7, f2
  fadd.s f2, f0, f1, rup
  fmv.x.w x8, f2
  fadd.s f2, f0, f1, rmm
  fmv.x.w x9, f2
  csrrwi x0, frm, 3
  fadd.s f2, f0, f1, dyn
  fmv.x.w x10, f2
  fadd.s f2, f0, f1
  fmv.x.w x11, f2
  lui x1, 260096
  addi x1, x1, 1
  fmv.w.x f0, x1
  lui x1, 260096
  addi x1, x1, -2
  fmv.w.x f1, x1
  lui x1, 784384
  addi x1, x1, 0
  fmv.w.x f2, x1
  fmadd.s f3, f0, f1, f2, rne
  fmv.x.w x12, f3
  lui x1, 262144
  addi x1, x1, 0
  fmv.w.x f0, x1
  lui x1, 263168
  addi x1, x1, 0
  fmv.w.x f1, x1
  lui x1, 264192
  addi x1, x1, 0
  fmv.w.x f2, x1
  fmadd.s f3, f0, f1, f2, rne
  fmv.x.w x13, f3
  fmsub.s f3, f0, f1, f2, rne
  fmv.x.w x14, f3
  fnmsub.s f3, f0, f1, f2, rne
  fmv.x.w x15, f3
  fnmadd.s f3, f0, f1, f2, rne
  fmv.x.w x16, f3
  fsqrt.s f3, f2, rne
  fmv.x.w x17, f3
  csrrs x18, fflags, x0

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x3f800000
# CHECK-DAG: GPR[6] = 0x3f800000
# CHECK-DAG: GPR[7] = 0x3f800000
# CHECK-DAG: GPR[8] = 0x3f800001
# CHECK-DAG: GPR[9] = 0x3f800001
# CHECK-DAG: GPR[10] = 0x3f800001
# CHECK-DAG: GPR[11] = 0x3f800001
# CHECK-DAG: GPR[12] = 0xffffffffa8800000
# CHECK-DAG: GPR[13] = 0x41200000
# CHECK-DAG: GPR[14] = 0x40000000
# CHECK-DAG: GPR[15] = 0xffffffffc0000000
# CHECK-DAG: GPR[16] = 0xffffffffc1200000
# CHECK-DAG: GPR[17] = 0x40000000
# CHECK-DAG: GPR[18] = 0x1
