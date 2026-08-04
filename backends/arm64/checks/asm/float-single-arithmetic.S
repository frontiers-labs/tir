// RUN: tir mc --march=armv8.0-a --filetype=obj-ascii %s | filecheck %s

.global float_single_arithmetic
float_single_arithmetic:
    fadd s0, s1, s2
    fsub s3, s4, s5
    fmul s6, s7, s8
    fdiv s9, s10, s11

// CHECK: .section .text
// CHECK-NEXT: float_single_arithmetic:
// CHECK-NEXT: [0x20, 0x28, 0x22, 0x1E]
// CHECK-NEXT: [0x83, 0x38, 0x25, 0x1E]
// CHECK-NEXT: [0xE6, 0x08, 0x28, 0x1E]
// CHECK-NEXT: [0x49, 0x19, 0x2B, 0x1E]
