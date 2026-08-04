// RUN: tir mc --march=armv8.0-a --filetype=obj-ascii %s | filecheck %s

.global float_single_convert
float_single_convert:
    scvtf s0, w1
    ucvtf s2, w3
    fcvtzs w4, s5
    fcvtzu w6, s7

// CHECK: .section .text
// CHECK-NEXT: float_single_convert:
// CHECK-NEXT: [0x20, 0x00, 0x22, 0x1E]
// CHECK-NEXT: [0x62, 0x00, 0x23, 0x1E]
// CHECK-NEXT: [0xA4, 0x00, 0x38, 0x1E]
// CHECK-NEXT: [0xE6, 0x00, 0x39, 0x1E]
