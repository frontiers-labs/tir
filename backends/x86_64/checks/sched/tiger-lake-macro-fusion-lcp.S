# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=10 %s | filecheck %s

# The 16-bit immediate compare keeps its length-changing-prefix decode cost
# when the branch shares its timing group.
.global f
f:
  cmp ax, 1
  je 8

# CHECK: Instructions:      20
# CHECK: Total Cycles:      29
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      cmp ax, 1
# CHECK-NEXT: 1      1      je 8
