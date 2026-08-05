# RUN: isasim --march=rv64ifdv --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: not isasim --march=rv64iv --entry=_start --until-pc=done %s

# Vector floating-point under e64/m1 (vl = 1 keeps vl*sew within the
# simulator's 64-bit register file): vfmv.v.f splats the f-register scalar,
# vfadd.vf adds it lane-wise, and vfmv.f.s moves element 0 back. The bit
# patterns are binary64: 0x3ff8... = 1.5, 0x4002... = 2.25, and the sum
# 0x400e... = 3.75. 216 = 0xd8 packs e64/m1/ta/ma.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x1, 0x3FF8
  slli x1, x1, 36
  fmv.d.x fa0, x1
  lui x2, 0x4002
  slli x2, x2, 36
  fmv.d.x fa1, x2
  vsetivli x0, 1, 216
  vfmv.v.f v1, fa0
  vfadd.vf v2, v1, fa1
  vfmv.f.s fa2, v2
  fmv.x.d x3, fa2

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0x400e000000000000
