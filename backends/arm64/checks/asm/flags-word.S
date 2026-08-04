// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global flags_word
flags_word:
    cmp w0, 7
    cmn w1, 8
    subs w2, w3, 9
    adds w4, w5, 10
    cmn w6, w7
    tst w8, w9

// CHECK: .section .text
// CHECK-NEXT: flags_word:
// CHECK-NEXT: [0x1F, 0x1C, 0x00, 0x71]
// CHECK-NEXT: [0x3F, 0x20, 0x00, 0x31]
// CHECK-NEXT: [0x62, 0x24, 0x00, 0x71]
// CHECK-NEXT: [0xA4, 0x28, 0x00, 0x31]
// CHECK-NEXT: [0xDF, 0x00, 0x07, 0x2B]
// CHECK-NEXT: [0x1F, 0x01, 0x09, 0x6A]
