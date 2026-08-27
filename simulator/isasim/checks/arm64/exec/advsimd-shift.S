# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Shifts by a signed per-lane amount, and the pairwise reductions. v2 shifts
# left by two and v3 (-2) shifts right by two, so the same instruction covers
# both directions; v4 is the most negative byte, where the saturating left
# shift pins. The pairwise results take their low half from the first source
# and their high half from the second.

.global done
done:
  add x0, x0, x0
.global _start
_start:
  movi v1.16b, 240
  movi v2.16b, 2
  movi v3.16b, 254
  movi v4.16b, 128
  sshl v5.16b, v1.16b, v2.16b
  sshl v6.16b, v1.16b, v3.16b
  ushl v7.16b, v1.16b, v3.16b
  srshl v8.16b, v1.16b, v3.16b
  urshl v9.16b, v1.16b, v3.16b
  sqshl v10.16b, v4.16b, v2.16b
  uqshl v11.16b, v1.16b, v2.16b
  sqrshl v12.16b, v1.16b, v3.16b
  uqrshl v13.16b, v1.16b, v3.16b
  addp v14.16b, v1.16b, v2.16b
  smaxp v15.16b, v1.16b, v2.16b
  uminp v16.16b, v1.16b, v2.16b

# CHECK: final registers:
# CHECK-DAG: VPR[1] = 0xf0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0
# CHECK-DAG: VPR[2] = 0x2020202020202020202020202020202
# CHECK-DAG: VPR[3] = 0xfefefefefefefefefefefefefefefefe
# CHECK-DAG: VPR[4] = 0x80808080808080808080808080808080
# CHECK-DAG: VPR[5] = 0xc0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0
# CHECK-DAG: VPR[6] = 0xfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfc
# CHECK-DAG: VPR[7] = 0x3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c
# CHECK-DAG: VPR[8] = 0xfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfc
# CHECK-DAG: VPR[9] = 0x3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c
# CHECK-DAG: VPR[10] = 0x80808080808080808080808080808080
# CHECK-DAG: VPR[11] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[12] = 0xfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfc
# CHECK-DAG: VPR[13] = 0x3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c
# CHECK-DAG: VPR[14] = 0x404040404040404e0e0e0e0e0e0e0e0
# CHECK-DAG: VPR[15] = 0x202020202020202f0f0f0f0f0f0f0f0
# CHECK-DAG: VPR[16] = 0x202020202020202f0f0f0f0f0f0f0f0
