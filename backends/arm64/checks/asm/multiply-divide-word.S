// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global multiply_divide_word
multiply_divide_word:
    madd w0, w1, w2, w3
    msub w4, w5, w6, w7
    mul w8, w9, w10
    mneg w11, w12, w13
    sdiv w14, w15, w16
    udiv w17, w18, w19

// CHECK: .section .text
// CHECK-NEXT: multiply_divide_word:
// CHECK-NEXT: [0x20, 0x0C, 0x02, 0x1B]
// CHECK-NEXT: [0xA4, 0x9C, 0x06, 0x1B]
// CHECK-NEXT: [0x28, 0x7D, 0x0A, 0x1B]
// CHECK-NEXT: [0x8B, 0xFD, 0x0D, 0x1B]
// CHECK-NEXT: [0xEE, 0x0D, 0xD0, 0x1A]
// CHECK-NEXT: [0x51, 0x0A, 0xD3, 0x1A]
