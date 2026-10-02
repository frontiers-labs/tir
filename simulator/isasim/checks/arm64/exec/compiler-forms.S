# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
.global done
done:
  add x0, x0, x0
.global _start
_start:
  movz x20, 32768
  movz x8, 16
  lslv x20, x20, x8
  movz x21, 4096
  add x20, x20, x21
  add x29, x20, 32
  mov w0, -1
  movk w0, 1, lsl 16
  lsl w0, w0, 1
  orr w0, w0, 1
  tst x0, 1
  cset w9, ne
  mov w1, -1
  csel w2, w0, w1, eq
  ccmp w9, w1, 4, eq
  cset w3, eq
  movz x10, 5
  movz x11, 7
  add x10, x10, x11, lsl 2
  subs x4, x10, x11
  movz x8, 1
  sub x5, x10, x8, lsl 5
  umaddl x6, w8, w1, x10
  fmov s31, 1.0
  fmov s30, 2.0
  fcmpe s31, s30
  cset w7, mi
  movz x15, 0
  movk x15, 32704, 1
  dup v2.4s, w15
  fcmpe s2, s31
  cset w14, vs
  fcmpe s31, s30
  dup v31.2d, x10
  dup v30.2d, x11
  stp q31, q30, [x20]
  ldp q30, q31, [x20]
  stur q31, [x29, -32]
  movk x8, 16
  movk x8, 1, 2
  ldr q1, [x20, xzr]
  movi v0.2d, 0
  str x10, [x20, w8, uxtw 3]
  ldur x12, [x20, 128]
# CHECK: final registers:
# CHECK-DAG: GPR[0] = 0x3ffff
# CHECK-DAG: GPR[2] = 0xffffffff
# CHECK-DAG: GPR[3] = 0x1
# CHECK-DAG: GPR[4] = 0x1a
# CHECK-DAG: GPR[5] = 0x1
# CHECK-DAG: GPR[6] = 0x100000020
# CHECK-DAG: GPR[7] = 0x1
# CHECK-DAG: GPR[12] = 0x21
# CHECK-DAG: VPR[0] = 0x0
# CHECK-DAG: VPR[1] = 0x70000000000000007
# CHECK-DAG: VPR[30] = 0x210000000000000021
# CHECK-DAG: VPR[31] = 0x70000000000000007
# CHECK-DAG: GPR[14] = 0x1
# CHECK-DAG: FPSR[0] = 0x1
