# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Single-precision results that land below the smallest normal. A subnormal is
# a representable value, not an underflow to zero, so every lane here has to
# survive the trip back into the lane's format. The expected values are what
# the instructions produce on hardware.

.global done
done:
  add x0, x0, x0
.global _start
_start:
  movi v0.4s, 0x01
  movi v1.4s, 0x03
  movi v2.4s, 0x00
  movi v3.4s, 0x02
  fadd v4.4s, v0.4s, v2.4s
  fadd v5.4s, v0.4s, v0.4s
  fadd v6.4s, v1.4s, v3.4s
  fsub v7.4s, v1.4s, v0.4s
  fmul v8.4s, v1.4s, v0.4s
  fabd v9.4s, v0.4s, v1.4s
  faddp v10.4s, v0.4s, v1.4s
  fmaxnm v11.4s, v0.4s, v1.4s
  fdiv v12.4s, v0.4s, v1.4s

# CHECK: final registers:
# CHECK-DAG: VPR[0] = 0x1000000010000000100000001
# CHECK-DAG: VPR[1] = 0x3000000030000000300000003
# CHECK-DAG: VPR[2] = 0x0
# CHECK-DAG: VPR[3] = 0x2000000020000000200000002
# CHECK-DAG: VPR[4] = 0x1000000010000000100000001
# CHECK-DAG: VPR[5] = 0x2000000020000000200000002
# CHECK-DAG: VPR[6] = 0x5000000050000000500000005
# CHECK-DAG: VPR[7] = 0x2000000020000000200000002
# CHECK-DAG: VPR[8] = 0x0
# CHECK-DAG: VPR[9] = 0x2000000020000000200000002
# CHECK-DAG: VPR[10] = 0x6000000060000000200000002
# CHECK-DAG: VPR[11] = 0x3000000030000000300000003
# CHECK-DAG: VPR[12] = 0x3eaaaaab3eaaaaab3eaaaaab3eaaaaab
