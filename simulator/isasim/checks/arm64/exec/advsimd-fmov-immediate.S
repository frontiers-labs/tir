# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# FMOV (vector, immediate) is the one modified-immediate form whose byte is not
# a lane pattern: `cmode = 1111` expands `abcdefgh` through VFPExpandImm into a
# binary64 value, where `b` and its complement stretch into the exponent. The
# cases below span both signs and both directions of that exponent. The
# expected values are what the instruction produces on hardware.

.global done
done:
  add x0, x0, x0
.global _start
_start:
  fmov v0.2d, 0
  fmov v1.2d, 128
  fmov v2.2d, 112
  fmov v3.2d, 47
  fmov v4.2d, 255
  fmov v5.2d, 1

# CHECK: final registers:
# CHECK-DAG: VPR[0] = 0x40000000000000004000000000000000
# CHECK-DAG: VPR[1] = 0xc000000000000000c000000000000000
# CHECK-DAG: VPR[2] = 0x3ff00000000000003ff0000000000000
# CHECK-DAG: VPR[3] = 0x402f000000000000402f000000000000
# CHECK-DAG: VPR[4] = 0xbfff000000000000bfff000000000000
# CHECK-DAG: VPR[5] = 0x40010000000000004001000000000000
