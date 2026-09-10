# RUN: isasim --march=rv64ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 0xc0000
  slli x1, x1, 32
  fmv.d.x f0, x1
  lui x1, 0xbff00
  slli x1, x1, 32
  fmv.d.x f1, x1
  flt.d x5, f0, f1
  fle.d x6, f0, f1
  feq.d x7, f0, f1
  lui x1, 0x80000
  slli x1, x1, 32
  fmv.d.x f0, x1
  fmv.d.x f1, x0
  feq.d x8, f0, f1
  flt.d x9, f0, f1
  csrrs x10, fflags, x0

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x1
# CHECK-DAG: GPR[6] = 0x1
# CHECK-DAG: GPR[7] = 0x0
# CHECK-DAG: GPR[8] = 0x1
# CHECK-DAG: GPR[9] = 0x0
# CHECK-DAG: GPR[10] = 0x0
