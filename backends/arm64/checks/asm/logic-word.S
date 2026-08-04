// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global logic_word
logic_word:
    and w0, w1, w2
    orr w3, w4, w5
    eor w6, w7, w8
    bic w9, w10, w11
    orn w12, w13, w14
    eon w15, w16, w17

// CHECK: .section .text
// CHECK-NEXT: logic_word:
// CHECK-NEXT: [0x20, 0x00, 0x02, 0x0A]
// CHECK-NEXT: [0x83, 0x00, 0x05, 0x2A]
// CHECK-NEXT: [0xE6, 0x00, 0x08, 0x4A]
// CHECK-NEXT: [0x49, 0x01, 0x2B, 0x0A]
// CHECK-NEXT: [0xAC, 0x01, 0x2E, 0x2A]
// CHECK-NEXT: [0x0F, 0x02, 0x31, 0x4A]
