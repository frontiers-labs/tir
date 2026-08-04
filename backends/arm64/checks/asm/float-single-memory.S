// RUN: tir mc --march=armv8.0-a --filetype=obj-ascii %s | filecheck %s

.global float_single_memory
float_single_memory:
    ldr s0, [x1, 4]
    str s2, [sp, 8]

// CHECK: .section .text
// CHECK-NEXT: float_single_memory:
// CHECK-NEXT: [0x20, 0x04, 0x40, 0xBD]
// CHECK-NEXT: [0xE2, 0x0B, 0x00, 0xBD]
