// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global f
f:
    add w0, w1, w2
    sub w3, w4, w5

// CHECK: .section .text
// CHECK-NEXT: f:
// CHECK-NEXT: [0x20, 0x00, 0x02, 0x0B]
// CHECK-NEXT: [0x83, 0x00, 0x05, 0x4B]
