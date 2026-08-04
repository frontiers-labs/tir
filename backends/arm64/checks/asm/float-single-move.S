// RUN: tir mc --march=armv8.0-a --filetype=obj-ascii %s | filecheck %s

.global float_single_move
float_single_move:
    fmov s0, s1
    fmov w2, s3
    fmov s4, w5

// CHECK: .section .text
// CHECK-NEXT: float_single_move:
// CHECK-NEXT: [0x20, 0x40, 0x20, 0x1E]
// CHECK-NEXT: [0x62, 0x00, 0x26, 0x1E]
// CHECK-NEXT: [0xA4, 0x00, 0x27, 0x1E]
