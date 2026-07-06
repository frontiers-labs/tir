# A extension: LR/SC, all nine AMOs at both widths, a register-aliasing AMO, and
# the FENCE / FENCE.I ordering fences. Harness contract: x1..x31 start at 0, x10
# holds the scratch base. Every atomic access is naturally aligned (LR/SC/AMO
# trap on misalignment, so the differential oracles only agree on aligned ones).
# Single hart, so each SC succeeds and each AMO returns the prior memory value.

# Scratch cursors into distinct aligned slots of the compared window.
  addi x5, x10, 0          # word AMO slot
  addi x6, x10, 16         # doubleword AMO slot
  addi x7, x10, 32         # LR/SC slot
  addi x28, x10, 48        # aliasing AMO slot

# Seed memory.
  addi x8, x0, 100
  sw   x8, 0(x5)
  sd   x8, 0(x6)
  addi x9, x0, 7
  sw   x9, 0(x7)

# LR/SC compare-and-increment. One hart, so the SC succeeds and writes 0.
  lr.w x11, (x7)
  addi x11, x11, 1
  sc.w x12, x11, (x7)      # x12 = 0, mem[x7] = 8

# Word AMOs, operand 9 in x13, then a negative operand -3 in x19.
  addi x13, x0, 9
  amoadd.w  x14, x13, (x5)
  amoswap.w x15, x13, (x5)
  amoxor.w  x16, x13, (x5)
  amoor.w   x17, x13, (x5)
  amoand.w  x18, x13, (x5)
  addi x19, x0, -3
  amomin.w  x20, x19, (x5)
  amomax.w  x21, x19, (x5)
  amominu.w x22, x19, (x5)
  amomaxu.w x23, x19, (x5)

# Doubleword AMOs on the 64-bit slot.
  amoadd.d  x24, x13, (x6)
  amoswap.d x25, x13, (x6)
  amomin.d  x26, x19, (x6)

# Register aliasing: rd == rs2. The operand must be read before rd is written, so
# the old value (5) lands in x30 and memory becomes 5 + 6 = 11.
  addi x29, x0, 5
  sw   x29, 0(x28)
  addi x30, x0, 6
  amoadd.w x30, x30, (x28)

# FENCE between two stores, then FENCE.I. Both are no-ops on one hart.
  sw   x8, 8(x5)
  fence 3, 3
  sw   x9, 12(x5)
  fence.i
