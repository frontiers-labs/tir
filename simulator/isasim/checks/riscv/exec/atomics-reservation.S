# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# LR/SC reservation edge cases. The reservation is keyed on (address, size):
# an SC to a different address or of a different width fails; a plain store by
# the same hart between LR and SC does not clear the reservation (documented
# policy); and a successful SC consumes it, so a second SC fails.
.global done
done:
  add x0, x0, x0
.global consume
consume:
  lr.w  t0, (a0)
  sw    t0, 0(a0)          # plain store: reservation survives
  sc.w  t3, t0, (a0)       # -> success, t3 = 0
  sc.w  t4, t0, (a0)       # reservation consumed -> t4 = 1
.global mismatch
mismatch:
  addi  a2, a0, 8
  lr.w  t0, (a0)
  sc.w  t1, t0, (a2)       # different address -> t1 = 1
  lr.w  t0, (a0)
  sc.d  t2, t0, (a0)       # different width -> t2 = 1
  lw    a6, 8(a0)          # the failed SCs wrote nothing
.global _start
_start:
  add x0, x0, x0

# CHECK: final registers:
# CHECK-DAG: GPR[6] = 0x1
# CHECK-DAG: GPR[7] = 0x1
# CHECK-DAG: GPR[28] = 0x0
# CHECK-DAG: GPR[29] = 0x1
# The default test window fills memory[a0 + i] = i & 0xff.
# CHECK-DAG: GPR[16] = 0xb0a0908
