# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=8 %s | filecheck %s

# `cvtsi2sd` writes only the low lane of its destination and the upper lane
# merges, so the write reads the old register: back-to-back converts into the
# same register serialize on the destination even though no source depends on
# it. Measured on hardware: 4.01 cyc/iteration for this exact block.

.global f
f:
    cvtsi2sd xmm0, rax

# CHECK: Total Cycles:      33
