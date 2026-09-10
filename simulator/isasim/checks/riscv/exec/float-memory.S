# RUN: isasim --march=rv64ifd --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  auipc x2, 0
  lui x1, 1047041
  addi x1, x1, 564
  fmv.w.x f0, x1
  fsw f0, 48(x2)
  flw f1, 48(x2)
  fmv.x.w x5, f1
  fmv.d.x f2, x1
  fsd f2, 56(x2)
  fld f3, 56(x2)
  fmv.x.d x6, f3
  add x0, x0, x0
  add x0, x0, x0
  add x0, x0, x0
  add x0, x0, x0
  add x0, x0, x0

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0xffffffffffa01234
# CHECK-DAG: GPR[6] = 0xffffffffffa01234
