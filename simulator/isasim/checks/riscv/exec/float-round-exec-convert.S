# RUN: isasim --march=rv64ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: isasim --march=rv32ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 262656
  addi x1, x1, 0
  fmv.w.x f0, x1
  fcvt.w.s x5, f0, rne
  fcvt.w.s x6, f0, rtz
  fcvt.w.s x7, f0, rdn
  fcvt.w.s x8, f0, rup
  fcvt.w.s x9, f0, rmm
  lui x1, 523264
  addi x1, x1, 0
  fmv.w.x f0, x1
  fcvt.w.s x10, f0, rne
  lui x1, 323584
  addi x1, x1, 0
  fmv.w.x f0, x1
  fcvt.w.s x11, f0, rne
  lui x1, 784384
  addi x1, x1, 0
  fmv.w.x f0, x1
  fcvt.wu.s x12, f0, rne
  csrrs x13, fflags, x0

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x2
# CHECK-DAG: GPR[6] = 0x2
# CHECK-DAG: GPR[7] = 0x2
# CHECK-DAG: GPR[8] = 0x3
# CHECK-DAG: GPR[9] = 0x3
# CHECK-DAG: GPR[10] = 0x7fffffff
# CHECK-DAG: GPR[11] = 0x7fffffff
# CHECK-DAG: GPR[12] = 0x0
# CHECK-DAG: GPR[13] = 0x11
