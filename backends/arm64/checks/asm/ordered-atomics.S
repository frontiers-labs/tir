// RUN: tir mc --march=armv8-a --filetype=obj-ascii %s | filecheck %s

.global ordered_atomics
ordered_atomics:
    ldar x0, [x1]
    ldar w2, [sp]
    stlr x3, [x4]
    stlr w5, [sp]

// CHECK: .section .text
// CHECK-NEXT: ordered_atomics:
// CHECK-NEXT: [0x20, 0xFC, 0xDF, 0xC8]
// CHECK-NEXT: [0xE2, 0xFF, 0xDF, 0x88]
// CHECK-NEXT: [0x83, 0xFC, 0x9F, 0xC8]
// CHECK-NEXT: [0xE5, 0xFF, 0x9F, 0x88]
