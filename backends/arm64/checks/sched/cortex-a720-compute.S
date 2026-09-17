# RUN: tir sched --march=armv8.0-a --mcpu=cortex-a720 --iterations=2 %s | filecheck %s

.global f
f:
    mul x0, x1, x2
    madd x3, x4, x5, x6
    sdiv w7, w8, w9
    udiv x10, x11, x12
    fadd s0, s1, s2
    fsub d3, d4, d5
    fmul s6, s7, s8
    fdiv s9, s10, s11
    fdiv d12, d13, d14
    scvtf s15, w16
    add v17.4s, v18.4s, v19.4s
    fmul v20.2d, v21.2d, v22.2d
    ldr x20, [x21, x22]

# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 2      1      mul x0, x1, x2
# CHECK-NEXT: 2      1      madd x3, x4, x5, x6
# CHECK-NEXT: 12     1      sdiv w7, w8, w9
# CHECK-NEXT: 20     1      udiv x10, x11, x12
# CHECK-NEXT: 2      1      fadd s0, s1, s2
# CHECK-NEXT: 2      1      fsub d3, d4, d5
# CHECK-NEXT: 3      1      fmul s6, s7, s8
# CHECK-NEXT: 8      1      fdiv s9, s10, s11
# CHECK-NEXT: 13     1      fdiv d12, d13, d14
# CHECK-NEXT: 3      1      scvtf s15, w16
# CHECK-NEXT: 2      1      add v17.4s, v18.4s, v19.4s
# CHECK-NEXT: 3      1      fmul v20.2d, v21.2d, v22.2d
# CHECK-NEXT: 4      1      ldr x20, [x21, x22]
# CHECK: Resources:
# CHECK-NEXT: [0]: Int0
# CHECK-NEXT: [1]: Int1
# CHECK-NEXT: [2]: Int2
# CHECK-NEXT: [3]: Int3
# CHECK-NEXT: [4]: FP0
# CHECK-NEXT: [5]: FP1
# CHECK-NEXT: [6]: IntDivide
# CHECK-NEXT: [7]: FloatDivide
# CHECK-NEXT: [8]: Load
# CHECK-NEXT: [9]: Unmodeled
# CHECK: Resource pressure per iteration:
# CHECK-NEXT: [0]   [1]   [2]   [3]   [4]   [5]   [6]   [7]   [8]   [9]
# CHECK-NEXT: 1.00  1.00  1.00  1.00  4.00  4.00  12.00 2.00  1.00  0.00
