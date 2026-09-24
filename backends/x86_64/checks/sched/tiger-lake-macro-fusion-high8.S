# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=10 %s | filecheck %s

# High-byte register forms carry the three-cycle prefix class through fusion.
.global f
f:
  add ah, al
  je 8

# CHECK: Instructions:      20
# CHECK: Total Cycles:      31
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 3      1      add ah, al
# CHECK-NEXT: 1      1      je 8
