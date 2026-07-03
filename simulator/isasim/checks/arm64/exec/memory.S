# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Unscaled loads/stores and register pairs. The default test memory window
# starts at 0x80001000 pre-filled with the byte pattern 00 01 02 ...; x1 is
# built to point at it with movz/lslv/add.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  movz x1, 32768
  add x2, x0, 16
  lslv x1, x1, x2
  movz x3, 4096
  add x1, x1, x3
  movz x4, 48879
  stur x4, [x1, 24]
  ldur x5, [x1, 24]
  ldurh x6, [x1, 24]
  ldurb x7, [x1, 24]
  ldursb x8, [x1, 24]
  ldur x9, [x1, 8]
  ldursw x10, [x1, 12]
  stp x4, x9, [x1, 32]
  ldp x11, x12, [x1, 32]
  add x13, x1, 64
  ldur x14, [x13, -40]
  sturb x4, [x1, 48]
  ldurb x15, [x1, 48]
  sturh x4, [x1, 56]
  ldurh x16, [x1, 56]

# The stur'd 0xbeef reads back whole, as a halfword, and as (un)signed bytes.
# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0xbeef
# CHECK-DAG: GPR[6] = 0xbeef
# CHECK-DAG: GPR[7] = 0xef
# CHECK-DAG: GPR[8] = 0xffffffffffffffef
# The pattern bytes 08..0f assemble little-endian; ldursw of 0c..0f stays
# positive.
# CHECK-DAG: GPR[9] = 0xf0e0d0c0b0a0908
# CHECK-DAG: GPR[10] = 0xf0e0d0c
# stp/ldp round-trips the pair; ldur accepts a negative offset.
# CHECK-DAG: GPR[11] = 0xbeef
# CHECK-DAG: GPR[12] = 0xf0e0d0c0b0a0908
# CHECK-DAG: GPR[14] = 0xbeef
# sturb/sturh truncate the stored value.
# CHECK-DAG: GPR[15] = 0xef
# CHECK-DAG: GPR[16] = 0xbeef
