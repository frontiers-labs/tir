# RUN: isasim --march=rv64ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: isasim --march=rv32ifd_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x1, x0, 0x3ff
  csrrw x1, fcsr, x1
  csrrs x5, fflags, x0
  csrrs x6, frm, x0
  csrrs x7, fcsr, x0
  csrrwi x8, frm, 2
  csrrci x9, fcsr, 16
  csrrsi x10, fflags, 16
  addi x2, x0, 0x120
  csrrs x2, fcsr, x2
  csrrs x11, fcsr, x0
  addi x3, x0, 0x41
  csrrc x3, fcsr, x3
  csrrs x12, fcsr, x0
  addi x4, x0, 0x1ff
  csrrw x0, fflags, x4
  csrrw x0, frm, x4
  csrrs x13, fflags, x0
  csrrs x14, frm, x0
  csrrs x15, fcsr, x0
  csrrwi x16, fcsr, 9
  csrrs x17, frm, x0
  csrrs x18, fflags, x0

# CHECK: final registers:
# CHECK-DAG: GPR[1] = 0x0
# CHECK-DAG: GPR[2] = 0x5f
# CHECK-DAG: GPR[3] = 0x7f
# CHECK-DAG: GPR[5] = 0x1f
# CHECK-DAG: GPR[6] = 0x7
# CHECK-DAG: GPR[7] = 0xff
# CHECK-DAG: GPR[8] = 0x7
# CHECK-DAG: GPR[9] = 0x5f
# CHECK-DAG: GPR[10] = 0xf
# CHECK-DAG: GPR[11] = 0x7f
# CHECK-DAG: GPR[12] = 0x3e
# CHECK-DAG: GPR[13] = 0x1f
# CHECK-DAG: GPR[14] = 0x7
# CHECK-DAG: GPR[15] = 0xff
# CHECK-DAG: GPR[16] = 0xff
# CHECK-DAG: GPR[17] = 0x0
# CHECK-DAG: GPR[18] = 0x9
