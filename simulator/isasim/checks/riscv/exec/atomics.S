# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Execution correctness for LR/SC and AMOs. The default test window fills
# memory[a0 + i] = i & 0xff, so the word at a0 reads 0x03020100 and the word at
# a1 (the window midpoint) reads the same. Blocks are declared bottom-up so
# `_start` lands at the base and control falls through loop -> tail -> done.
#
# `_start` falls into `loop`, an LR/SC compare-and-increment that succeeds on the
# first try (one hart), leaving t1 = 0 (SC success) and memory[a0] = 0x03020101.
# `tail` then shows SC fails (t2 = 1) once the reservation is consumed, and the
# AMOs each return the OLD memory value.
.global done
done:
  add x0, x0, x0
.global tail
tail:
  sc.w      t2, t0, (a0)     # no live reservation -> failure -> t2 = 1
  lw        a6, 0(a0)        # observe the incremented word -> 0x03020101
  addi      t3, x0, 10
  amoadd.w  a2, t3, (a1)     # old 0x03020100 -> a2; mem += 10
  amoswap.w a3, t3, (a1)     # old 0x0302010a -> a3; mem = 10
  addi      t4, x0, -5
  amomin.w  a4, t4, (a1)     # signed min(10, -5): old 10 -> a4; mem = -5
  amomaxu.w a5, t4, (a1)     # unsigned max: old -5 -> a5 (sign-extended)
.global loop
loop:
  lr.w      t0, (a0)
  addi      t0, t0, 1
  sc.w      t1, t0, (a0)
  bne       t1, x0, -12
.global _start
_start:
  add       x0, x0, x0

# CHECK: final registers:
# SC success writes 0, the reservation-less retry writes 1.
# CHECK-DAG: GPR[6] = 0x0
# CHECK-DAG: GPR[7] = 0x1
# The AMOs each return the prior memory contents.
# CHECK-DAG: GPR[12] = 0x3020100
# CHECK-DAG: GPR[13] = 0x302010a
# CHECK-DAG: GPR[14] = 0xa
# CHECK-DAG: GPR[15] = 0xfffffffffffffffb
# The LR/SC loop incremented the word in memory.
# CHECK-DAG: GPR[16] = 0x3020101
