// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global memory_narrow_word
memory_narrow_word:
    ldrb w0, [x1, 1]
    ldrh w2, [x3, 2]
    ldrsb w4, [x5, 3]
    ldrsh w6, [x7, 4]
    strb w8, [x9, 5]
    strh w10, [x11, 6]

// CHECK: .section .text
// CHECK-NEXT: memory_narrow_word:
// CHECK-NEXT: [0x20, 0x04, 0x40, 0x39]
// CHECK-NEXT: [0x62, 0x04, 0x40, 0x79]
// CHECK-NEXT: [0xA4, 0x0C, 0xC0, 0x39]
// CHECK-NEXT: [0xE6, 0x08, 0xC0, 0x79]
// CHECK-NEXT: [0x28, 0x15, 0x00, 0x39]
// CHECK-NEXT: [0x6A, 0x0D, 0x00, 0x79]
