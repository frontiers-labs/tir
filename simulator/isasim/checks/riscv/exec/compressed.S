# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Compressed instructions execute with 2-byte PC stepping, interleaved with
# full-width instructions: a0 = (5 + 7) = 12, a1 = (12 + 12) << 2 = 96,
# a2 = sp + 16 = 16, x6 = 3.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  c.li a0, 5
  c.addi a0, 7
  c.mv a1, a0
  c.add a1, a0
  c.slli a1, 2
  c.addi4spn a2, sp, 16
  addi x6, x0, 3
  c.nop
  c.andi s0, 0

# CHECK: final registers:
# CHECK-DAG: GPR[6] = 0x3
# CHECK-DAG: GPR[10] = 0xc
# CHECK-DAG: GPR[11] = 0x60
# CHECK-DAG: GPR[12] = 0x10
