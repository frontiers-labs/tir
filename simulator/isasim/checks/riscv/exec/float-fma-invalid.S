# RUN: isasim --march=rv64ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  fmv.w.x f0, x0
  lui x1, 0x7f800
  fmv.w.x f1, x1
  lui x1, 0x7fc00
  fmv.w.x f2, x1
  csrrwi x0, fflags, 0
  fmadd.s f3, f0, f1, f2, rne
  fclass.s x5, f3
  csrrs x6, fflags, x0
  fmv.d.x f0, x0
  lui x1, 0x7ff00
  slli x1, x1, 32
  fmv.d.x f1, x1
  lui x1, 0x7ff80
  slli x1, x1, 32
  fmv.d.x f2, x1
  csrrwi x0, fflags, 0
  fmadd.d f3, f0, f1, f2, rne
  fclass.d x7, f3
  csrrs x8, fflags, x0

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x200
# CHECK-DAG: GPR[6] = 0x10
# CHECK-DAG: GPR[7] = 0x200
# CHECK-DAG: GPR[8] = 0x10
