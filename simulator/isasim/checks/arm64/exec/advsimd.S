# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Advanced SIMD lane semantics: the same arrangement of the same registers is
# read as bytes, halfwords or words depending on the instruction, so `add` on
# `.4s` and on `.16b` differ only where a lane carry would cross a byte. The
# expected values are what the instructions produce on hardware.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  movi v1.16b, 18
  movi v2.4s, 3
  add  v3.4s, v1.4s, v2.4s
  add  v4.16b, v1.16b, v2.16b
  sub  v5.8h, v1.8h, v2.8h
  mul  v6.4s, v1.4s, v2.4s
  and  v7.16b, v1.16b, v2.16b


# CHECK: final registers:
# CHECK-DAG: VPR[1] = 0x12121212121212121212121212121212
# CHECK-DAG: VPR[2] = 0x3000000030000000300000003
# CHECK-DAG: VPR[3] = 0x12121215121212151212121512121215
# CHECK-DAG: VPR[4] = 0x12121215121212151212121512121215
# CHECK-DAG: VPR[5] = 0x1212120f1212120f1212120f1212120f
# CHECK-DAG: VPR[6] = 0x36363636363636363636363636363636
# CHECK-DAG: VPR[7] = 0x2000000020000000200000002

