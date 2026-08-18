# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --timing --machine=rv64-ooo --predictor=not-taken --konata - %s | filecheck %s

# With the always-not-taken predictor, the two taken iterations of the loop
# back-edge both mispredict. The Konata log annotates the branch, refills the
# pipeline through a flush fetch, and shows the squashed wrong-path
# (fall-through) instructions fetched under the branch shadow.
.global done
done:
  add x0, x0, x0
.global tail
tail:
  add  x2, x1, x1
.global loop
loop:
  addi x1, x1, -1
  bne  x1, x0, -4
.global _start
_start:
  addi x1, x0, 3

# CHECK: Kanata{{\t}}0004
# The first taken back-edge is annotated as a mispredict...
# CHECK: L{{\t}}{{[0-9]+}}{{\t}}1{{\t}}mispredicted branch
# ...its shadow is filled with squashed wrong-path fall-through work...
# CHECK: speculative (wrong path)
# ...and the redirected fetch is tagged as a flush.
# CHECK: flush
# CHECK: timing[OutOfOrderCore / always-not-taken]: 8 instructions, {{[0-9]+}} cycles, IPC {{[0-9.]+}}, 2 mispredicts
