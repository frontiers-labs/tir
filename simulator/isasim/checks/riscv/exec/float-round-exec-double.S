# RUN: isasim --march=rv64ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x1, x0, 64
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  fmv.d.x f0, x1
  addi x1, x0, 64
  slli x1, x1, 8
  addi x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  fmv.d.x f1, x1
  addi x1, x0, 64
  slli x1, x1, 8
  addi x1, x1, 16
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  fmv.d.x f2, x1
  fmadd.d f3, f0, f1, f2, rne
  fmv.x.d x5, f3
  fmsub.d f3, f0, f1, f2, rne
  fmv.x.d x6, f3
  fnmsub.d f3, f0, f1, f2, rne
  fmv.x.d x7, f3
  fnmadd.d f3, f0, f1, f2, rne
  fmv.x.d x8, f3
  fsqrt.d f3, f2, rne
  fmv.x.d x9, f3
  addi x1, x0, 63
  slli x1, x1, 8
  addi x1, x1, 240
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  addi x1, x1, 1
  fmv.d.x f0, x1
  addi x1, x0, 63
  slli x1, x1, 8
  addi x1, x1, 239
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
  addi x1, x1, 254
  fmv.d.x f1, x1
  addi x1, x0, 191
  slli x1, x1, 8
  addi x1, x1, 240
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  slli x1, x1, 8
  fmv.d.x f2, x1
  fmadd.d f3, f0, f1, f2, rne
  fmv.x.d x10, f3
  csrrwi x0, fflags, 0
  fsqrt.d f3, f2, rne
  fmv.x.d x11, f3
  csrrs x12, fflags, x0

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x4024000000000000
# CHECK-DAG: GPR[6] = 0x4000000000000000
# CHECK-DAG: GPR[7] = 0xc000000000000000
# CHECK-DAG: GPR[8] = 0xc024000000000000
# CHECK-DAG: GPR[9] = 0x4000000000000000
# CHECK-DAG: GPR[10] = 0xb970000000000000
# CHECK-DAG: GPR[11] = 0x7ff8000000000000
# CHECK-DAG: GPR[12] = 0x10
