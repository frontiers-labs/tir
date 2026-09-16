# RUN: tir sched --march=rv64im --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

.global f
f:
    mul x10, x10, x11
    mulw x12, x12, x13
    mulh x14, x14, x15
    mulhu x16, x16, x17
    mulhsu x18, x18, x19

# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 3      1      mul x10, x10, x11
# CHECK-NEXT: 2      1      mulw x12, x12, x13
# CHECK-NEXT: 3      1      mulh x14, x14, x15
# CHECK-NEXT: 3      1      mulhu x16, x16, x17
# CHECK-NEXT: 3      1      mulhsu x18, x18, x19
