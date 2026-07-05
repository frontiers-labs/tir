# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --timing --machine=rv64-ooo --mem-model=test --prefetcher=next-line %s | filecheck %s

# The same sequential line walk as cache.S, but with the next-line prefetcher
# enabled. Each demand access speculatively fetches the following line; those
# fills that find a free L1D MSHR install ahead of the demand, so some lines that
# cold-missed in cache.S (64 L1D misses) now hit. The prefetches issued here race
# the still-outstanding demand stream, so every useful one is also late.
.global done
done:
  add x0, x0, x0
.global tail
tail:
  lw   a5, 0(a0)         # reload line 0
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

# L1D misses fall from 64 (cache.S) to 61 as three prefetched lines hit; L2 and
# DRAM demand traffic drop in step. All three prefetches arrive late.
# CHECK: 259 instructions
# CHECK: mem[l1d]: 65 accesses, 4 hits, 61 misses
# CHECK: mem[l2]: 62 accesses, 0 hits, 62 misses
# CHECK: mem[dram]: 62 accesses
# CHECK: mem[pf]: 3 issued, 3 useful, 3 late
