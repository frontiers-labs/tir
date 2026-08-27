# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Bitwise select and insert, the accumulating multiplies, polynomial multiply,
# and the saturating doubling multiplies. Everything in the first group reads
# the destination, so each is seeded with an `orr` of a source onto itself.
# v12 holds the most negative halfword, the one input where the doubling
# multiply saturates.

.global done
done:
  add x0, x0, x0
.global _start
_start:
  movi v1.16b, 240
  movi v2.16b, 51
  movi v3.16b, 15
  movz x1, 32768
  dup v12.8h, w1
  orr v4.16b, v3.16b, v3.16b
  bsl v4.16b, v1.16b, v2.16b
  orr v5.16b, v1.16b, v1.16b
  bit v5.16b, v2.16b, v3.16b
  orr v6.16b, v1.16b, v1.16b
  bif v6.16b, v2.16b, v3.16b
  movi v7.16b, 3
  orr v8.16b, v2.16b, v2.16b
  mla v8.16b, v1.16b, v7.16b
  orr v9.16b, v2.16b, v2.16b
  mls v9.16b, v1.16b, v7.16b
  orr v10.16b, v2.16b, v2.16b
  saba v10.16b, v1.16b, v7.16b
  pmul v11.16b, v1.16b, v7.16b
  sqdmulh v13.8h, v12.8h, v12.8h
  sqrdmulh v14.8h, v12.8h, v12.8h

# CHECK: final registers:
# CHECK-DAG: VPR[4] = 0x30303030303030303030303030303030
# CHECK-DAG: VPR[5] = 0xf3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3
# CHECK-DAG: VPR[6] = 0x30303030303030303030303030303030
# CHECK-DAG: VPR[8] = 0x3030303030303030303030303030303
# CHECK-DAG: VPR[9] = 0x63636363636363636363636363636363
# CHECK-DAG: VPR[10] = 0x46464646464646464646464646464646
# CHECK-DAG: VPR[11] = 0x10101010101010101010101010101010
# CHECK-DAG: VPR[13] = 0x7fff7fff7fff7fff7fff7fff7fff7fff
# CHECK-DAG: VPR[14] = 0x7fff7fff7fff7fff7fff7fff7fff7fff
