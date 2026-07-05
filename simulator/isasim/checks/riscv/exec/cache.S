# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --timing --machine=rv64-ooo --mem-model=test %s | filecheck %s

# A loop that loads one word from each of the 64 distinct 64-byte lines spanning
# the default 4 KiB test window (a0), then reloads the first line. Against the
# `test` memory model (L1D 4 KiB / 4-way / 64 lines) every line is a cold miss on
# the first pass; the reload of line 0 — still resident — is the single hit.
#
# The asm parser lays symbols out in reverse source order, so the blocks are
# declared bottom-up to put `_start` at the base and `done` (the stop sentinel)
# on top as the fallthrough.
.global done
done:
  add x0, x0, x0
.global tail
tail:
  lw   a5, 0(a0)         # reload line 0: an L1D hit
.global loop
loop:
  lw   a4, 0(a3)
  addi a3, a3, 64        # next line
  addi a2, a2, -1
  bne  a2, zero, -12     # back to the lw
.global _start
_start:
  addi a2, zero, 64      # line count
  add  a3, a0, zero      # cursor = base of the test window

# The whole code footprint fits in one 64-byte line, so the front end takes a
# single L1I miss; L2 then sees it plus the 64 L1D misses (65 total).
# CHECK: 259 instructions
# CHECK: mem[l1i]: 1 accesses, 0 hits, 1 misses
# CHECK: mem[l1d]: 65 accesses, 1 hits, 64 misses
# CHECK: mem[l2]: 65 accesses, 0 hits, 65 misses
# CHECK: mem[dram]: 65 accesses
# CHECK: mem[writebacks]: 0
