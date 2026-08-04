// RUN: tir mc --march=armv8.0-a --filetype=obj-ascii %s | filecheck %s

.global float_single_compare
float_single_compare:
    fcmp s0, s1

// CHECK: .section .text
// CHECK-NEXT: float_single_compare:
// CHECK-NEXT: [0x00, 0x20, 0x21, 0x1E]
