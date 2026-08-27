# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Halving, rounding-halving and saturating byte arithmetic. v1 is -16 as a
# signed byte and 240 as an unsigned one, so every signed/unsigned pair here
# disagrees; v3 is the most negative byte, where the saturating forms pin.
# The expected values are what the instructions produce on hardware.

.global done
done:
  add x0, x0, x0
.global _start
_start:
  movi v1.16b, 240
  movi v2.16b, 48
  movi v3.16b, 128
  shadd v4.16b, v1.16b, v2.16b
  uhadd v5.16b, v1.16b, v2.16b
  srhadd v6.16b, v1.16b, v2.16b
  urhadd v7.16b, v1.16b, v2.16b
  shsub v8.16b, v1.16b, v2.16b
  uhsub v9.16b, v1.16b, v2.16b
  sqadd v10.16b, v1.16b, v2.16b
  uqadd v11.16b, v1.16b, v2.16b
  sqadd v12.16b, v3.16b, v3.16b
  sqsub v13.16b, v3.16b, v2.16b
  uqsub v14.16b, v2.16b, v1.16b
  sabd v15.16b, v1.16b, v2.16b
  uabd v16.16b, v1.16b, v2.16b

# CHECK: final registers:
# CHECK-DAG: VPR[1] = 0xf0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0
# CHECK-DAG: VPR[2] = 0x30303030303030303030303030303030
# CHECK-DAG: VPR[3] = 0x80808080808080808080808080808080
# CHECK-DAG: VPR[4] = 0x10101010101010101010101010101010
# CHECK-DAG: VPR[5] = 0x90909090909090909090909090909090
# CHECK-DAG: VPR[6] = 0x10101010101010101010101010101010
# CHECK-DAG: VPR[7] = 0x90909090909090909090909090909090
# CHECK-DAG: VPR[8] = 0xe0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0
# CHECK-DAG: VPR[9] = 0x60606060606060606060606060606060
# CHECK-DAG: VPR[10] = 0x20202020202020202020202020202020
# CHECK-DAG: VPR[11] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[12] = 0x80808080808080808080808080808080
# CHECK-DAG: VPR[13] = 0x80808080808080808080808080808080
# CHECK-DAG: VPR[14] = 0x0
# CHECK-DAG: VPR[15] = 0x40404040404040404040404040404040
# CHECK-DAG: VPR[16] = 0xc0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0
