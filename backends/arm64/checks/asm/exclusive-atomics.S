// RUN: tir mc --march=armv8-a --filetype=obj-ascii %s | filecheck %s

.global atomics
atomics:
    ldxr x0, [x1]
    ldxr w2, [x3]
    ldaxr x4, [sp]
    stxr w5, x6, [x7]
    stxr w8, w9, [x10]
    stlxr w11, x12, [sp]

// CHECK: .section .text
// CHECK-NEXT: atomics:
// CHECK-NEXT: [0x20, 0x7C, 0x5F, 0xC8]
// CHECK-NEXT: [0x62, 0x7C, 0x5F, 0x88]
// CHECK-NEXT: [0xE4, 0xFF, 0x5F, 0xC8]
// CHECK-NEXT: [0xE6, 0x7C, 0x05, 0xC8]
// CHECK-NEXT: [0x49, 0x7D, 0x08, 0x88]
// CHECK-NEXT: [0xEC, 0xFF, 0x0B, 0xC8]
