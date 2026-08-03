# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    cmp r8, r9
    test r10, r11

# CHECK: Total Cycles:      68
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      cmp r8, r9
# CHECK-NEXT: 1      1      test r10, r11
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: 0.34  0.33  0.33  -     cmp r8, r9
# CHECK-NEXT: 0.33  0.34  0.33  -     test r10, r11
