# RUN: isasim --march=rv64ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 0x7fc00
  fmv.w.x f0, x1
  lui x1, 0x7f800
  addi x1, x1, 1
  fmv.w.x f1, x1
  fmv.w.x f2, x0
  csrrwi x0, fflags, 1
  feq.s x0, f0, f0
  csrrs x5, fflags, x0
  flt.s x0, f0, f2
  csrrs x6, fflags, x0
  csrrwi x0, fflags, 0
  feq.s x0, f1, f2
  csrrs x7, fflags, x0
  csrrwi x0, fflags, 0
  fle.s x0, f2, f0
  csrrs x8, fflags, x0
  csrrwi x0, fflags, 0
  fmin.s f3, f0, f2
  csrrs x9, fflags, x0
  fmax.s f3, f1, f2
  csrrs x10, fflags, x0
  csrrwi x0, fflags, 0
  fclass.s x0, f1
  fsgnj.s f3, f1, f2
  csrrs x11, fflags, x0
  lui x1, 0x7ff00
  slli x1, x1, 32
  addi x1, x1, 1
  fmv.d.x f0, x1
  fmv.d.x f2, x0
  fmin.d f3, f0, f2
  csrrs x12, fflags, x0
  csrrwi x0, fflags, 0
  feq.d x0, f0, f2
  csrrs x13, fflags, x0
  lui x1, 0x7ff80
  slli x1, x1, 32
  fmv.d.x f0, x1
  csrrwi x0, fflags, 0
  feq.d x0, f0, f0
  csrrs x14, fflags, x0
  flt.d x0, f0, f2
  csrrs x15, fflags, x0
  csrrwi x0, fflags, 0
  fle.d x0, f2, f0
  csrrs x16, fflags, x0

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x1
# CHECK-DAG: GPR[6] = 0x11
# CHECK-DAG: GPR[7] = 0x10
# CHECK-DAG: GPR[8] = 0x10
# CHECK-DAG: GPR[9] = 0x0
# CHECK-DAG: GPR[10] = 0x10
# CHECK-DAG: GPR[11] = 0x0
# CHECK-DAG: GPR[12] = 0x10
# CHECK-DAG: GPR[13] = 0x10
# CHECK-DAG: GPR[14] = 0x0
# CHECK-DAG: GPR[15] = 0x10
# CHECK-DAG: GPR[16] = 0x10
