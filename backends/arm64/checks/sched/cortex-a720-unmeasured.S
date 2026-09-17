# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=2 %s | filecheck %s

.global f
f:
    b 0
    nop
    smulh x13, x14, x15
    fmov d0, d1
    fcmp d2, d3
    and v2.16b, v3.16b, v4.16b
    mul v5.4s, v6.4s, v7.4s
    mla v10.4s, v11.4s, v12.4s
    ldr d8, [x0, 8]
    ldr q9, [x1, 16]
    str w2, [sp, 8]

# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 2      1      b 0
# CHECK-NEXT: 2      1      nop
# CHECK-NEXT: 3      1      smulh x13, x14, x15
# CHECK-NEXT: 2      1      fmov d0, d1
# CHECK-NEXT: 2      1      fcmp d2, d3
# CHECK-NEXT: 2      1      and v2.16b, v3.16b, v4.16b
# CHECK-NEXT: 3      1      mul v5.4s, v6.4s, v7.4s
# CHECK-NEXT: 3      1      mla v10.4s, v11.4s, v12.4s
# CHECK-NEXT: 4      1      ldr d8, [x0, 8]
# CHECK-NEXT: 4      1      ldr q9, [x1, 16]
# CHECK-NEXT: 2      1      str w2, [sp, 8]
# CHECK: Resource pressure per iteration:
# CHECK-NEXT: [0]   [1]   [2]   [3]   [4]   [5]   [6]   [7]   [8]   [9]
# CHECK-NEXT: 0.00  0.00  0.00  0.00  0.00  0.00  0.00  0.00  0.00  11.00
