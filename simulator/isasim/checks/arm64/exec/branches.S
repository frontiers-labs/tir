# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Conditional, compare-and-branch, and test-bit branches. Branch immediates
# are word offsets; a taken `2` lands on the block after the fall-through
# block, whose single instruction overwrites the register's initial 1 with 9.
# Blocks are laid out in reverse declaration order, so execution flows bottom
# to top through this file: _start, f1, t1, ..., t8, done. x1 = 5 (0b101),
# x2 = 7.
.global done
done:
  add x0, x0, x0
.global t8
t8:
  cmp x1, x2
  cset x20, lt
  cset x21, ge
  csel x22, x1, x2, lt
  csel x23, x1, x2, ge
.global f8
f8:
  add x17, x0, 9
.global t7
t7:
  add x17, x0, 1
  b.hi 2
.global f7
f7:
  add x16, x0, 9
.global t6
t6:
  add x16, x0, 1
  cmp x1, x1
  b.le 2
.global f6
f6:
  add x15, x0, 9
.global t5
t5:
  add x15, x0, 1
  tbnz x1, 2, 2
.global f5
f5:
  add x14, x0, 9
.global t4
t4:
  add x14, x0, 1
  tbz x1, 1, 2
.global f4
f4:
  add x13, x0, 9
.global t3
t3:
  add x13, x0, 1
  cbnz x0, 2
.global f3
f3:
  add x12, x0, 9
.global t2
t2:
  add x12, x0, 1
  cbz x0, 2
.global f2
f2:
  add x11, x0, 9
.global t1
t1:
  add x11, x0, 1
  cmp x2, x1
  b.gt 2
.global f1
f1:
  add x10, x0, 9
.global _start
_start:
  add x1, x0, 5
  add x2, x0, 7
  add x10, x0, 1
  cmp x1, x2
  b.gt 2

# 5 > 7 falls through (x10 = 9); 7 > 5 is taken (x11 = 1).
# CHECK: final registers:
# CHECK-DAG: GPR[10] = 0x9
# CHECK-DAG: GPR[11] = 0x1
# cbz on x0 (zero) is taken; cbnz on x0 is not.
# CHECK-DAG: GPR[12] = 0x1
# CHECK-DAG: GPR[13] = 0x9
# Bit 1 of 0b101 is clear (tbz taken), bit 2 is set (tbnz taken).
# CHECK-DAG: GPR[14] = 0x1
# CHECK-DAG: GPR[15] = 0x1
# 5 vs 5: le holds, hi does not.
# CHECK-DAG: GPR[16] = 0x1
# CHECK-DAG: GPR[17] = 0x9
# cmp 5, 7: lt holds; csel picks rn on lt, rm on ge.
# CHECK-DAG: GPR[20] = 0x1
# CHECK-DAG: GPR[21] = 0x0
# CHECK-DAG: GPR[22] = 0x5
# CHECK-DAG: GPR[23] = 0x7
