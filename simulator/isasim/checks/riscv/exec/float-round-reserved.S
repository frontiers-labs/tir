# RUN: isasim --march=rv64ifd_zicsr --entry=rm5 --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: isasim --march=rv64ifd_zicsr --entry=rm6 --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: isasim --march=rv64ifd_zicsr --entry=rm7 --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global rm5
rm5:
  csrrwi x0, frm, 5
  fmadd.s f0, f0, f0, f0
  addi x5, x0, 1
.global rm6
rm6:
  csrrwi x0, frm, 6
  fcvt.w.d x5, f0
  addi x5, x0, 1
.global rm7
rm7:
  csrrwi x0, frm, 7
  fsqrt.d f0, f0
  addi x5, x0, 1

# CHECK: trap: cause=2
# CHECK: final registers:
# CHECK-NOT: GPR[5]
