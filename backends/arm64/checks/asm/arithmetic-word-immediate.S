// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global arithmetic_word_immediate
arithmetic_word_immediate:
    add w0, w1, 42
    sub w2, w3, 4095

// CHECK: .section .text
// CHECK-NEXT: arithmetic_word_immediate:
// CHECK-NEXT: [0x20, 0xA8, 0x00, 0x11]
// CHECK-NEXT: [0x62, 0xFC, 0x3F, 0x51]
