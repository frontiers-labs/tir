# RUN: tir mc --march=rv32i_zba %s | filecheck %s

.global f
f:
    sh1add x10, x11, x12
    sh2add x10, x11, x12
    sh3add x10, x11, x12

# CHECK: sh1add x10, x11, x12
# CHECK-NEXT: sh2add x10, x11, x12
# CHECK-NEXT: sh3add x10, x11, x12
