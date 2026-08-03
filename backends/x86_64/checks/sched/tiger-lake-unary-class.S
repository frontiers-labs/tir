# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    neg r8
    not r9

# CHECK: Total Cycles:      101
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      neg r8
# CHECK-NEXT: 1      1      not r9
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: 0.34  0.33  0.33  -     neg r8
# CHECK-NEXT: 0.33  0.34  0.33  -     not r9
