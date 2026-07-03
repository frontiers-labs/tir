# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Inverted logic (bic/orn/eon), move-wide (movn/movk), rotates and immediate
# shifts, sign-extends, and flag-driven cset/csel: 12 = 0b1100, 10 = 0b1010.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  add x1, x0, 12
  add x2, x0, 10
  bic x3, x1, x2
  orn x4, x1, x2
  eon x5, x1, x2
  movn x6, 0
  movz x7, 4660
  add x8, x0, 16
  lslv x7, x7, x8
  movk x7, 22136
  add x9, x0, 15
  add x10, x0, 4
  rorv x11, x9, x10
  lsr x12, x7, 8
  asr x13, x6, 8
  add x14, x0, 128
  sxtb x15, x14
  movz x16, 32768
  sxth x17, x16
  lslv x18, x16, x8
  sxtw x19, x18
  cmp x1, x2
  cset x20, gt
  cset x21, le
  cset x22, hi
  csel x23, x1, x2, lt
  csel x24, x1, x2, ge
  tst x0, x0
  cset x25, eq
  cmn x6, x1
  cset x26, hs

# bic: 12 & ~10 = 4; orn: 12 | ~10; eon: ~(12 ^ 10) = ~6.
# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0x4
# CHECK-DAG: GPR[4] = 0xfffffffffffffffd
# CHECK-DAG: GPR[5] = 0xfffffffffffffff9
# CHECK-DAG: GPR[6] = 0xffffffffffffffff
# movz/lslv/movk compose 0x12345678; ror of 0xf by 4 wraps the low nibble.
# CHECK-DAG: GPR[7] = 0x12345678
# CHECK-DAG: GPR[11] = 0xf000000000000000
# CHECK-DAG: GPR[12] = 0x123456
# CHECK-DAG: GPR[13] = 0xffffffffffffffff
# CHECK-DAG: GPR[15] = 0xffffffffffffff80
# CHECK-DAG: GPR[17] = 0xffffffffffff8000
# CHECK-DAG: GPR[19] = 0xffffffff80000000
# cmp 12, 10: gt/hi hold, le does not; csel picks by n/v.
# CHECK-DAG: GPR[20] = 0x1
# CHECK-DAG: GPR[21] = 0x0
# CHECK-DAG: GPR[22] = 0x1
# CHECK-DAG: GPR[23] = 0xa
# CHECK-DAG: GPR[24] = 0xc
# tst 0, 0 sets z; cmn -1, 12 carries out.
# CHECK-DAG: GPR[25] = 0x1
# CHECK-DAG: GPR[26] = 0x1
