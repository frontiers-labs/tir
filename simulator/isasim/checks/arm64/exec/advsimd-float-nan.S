# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# NaN handling in the float three-same operations. Every lane of v1 is a
# negative quiet NaN, so each instruction here has to say which NaN comes
# out: the arithmetic propagates the operand unchanged rather than returning
# a canonical NaN, FMAXNM/FMINNM instead return the other operand, and the
# compares just answer false. The expected values are what the instructions
# produce on hardware.

.global done
done:
  add x0, x0, x0
.global _start
_start:
  movi v1.16b, 255
  movi v2.16b, 1
  movi v3.16b, 128
  fadd v4.4s, v1.4s, v2.4s
  fsub v5.4s, v1.4s, v2.4s
  fmul v6.4s, v1.4s, v2.4s
  fdiv v7.4s, v1.4s, v2.4s
  fabd v8.4s, v1.4s, v2.4s
  fmax v9.4s, v1.4s, v2.4s
  fmin v10.4s, v1.4s, v2.4s
  fmaxnm v11.4s, v1.4s, v2.4s
  fminnm v12.4s, v1.4s, v2.4s
  orr v13.16b, v3.16b, v3.16b
  fmla v13.4s, v1.4s, v2.4s
  fmulx v14.4s, v1.4s, v2.4s
  frecps v15.4s, v1.4s, v2.4s
  fcmeq v16.4s, v1.4s, v2.4s

# CHECK: final registers:
# CHECK-DAG: VPR[4] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[5] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[6] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[7] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[8] = 0x7fffffff7fffffff7fffffff7fffffff
# CHECK-DAG: VPR[9] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[10] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[11] = 0x1010101010101010101010101010101
# CHECK-DAG: VPR[12] = 0x1010101010101010101010101010101
# CHECK-DAG: VPR[13] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[14] = 0xffffffffffffffffffffffffffffffff
# CHECK-DAG: VPR[15] = 0x7fffffff7fffffff7fffffff7fffffff
# CHECK-DAG: VPR[16] = 0x0
