// RUN: tir mc --march=armv8.1-a --filetype=obj-ascii %s | filecheck %s

.global lse_clear
lse_clear:
    ldclr x0, x1, [x2]
    ldclra w3, w4, [x5]
    ldclrl x6, x7, [x8]
    ldclral w9, w10, [sp]

// CHECK: .section .text
// CHECK-NEXT: lse_clear:
// CHECK-NEXT: [0x41, 0x10, 0x20, 0xF8]
// CHECK-NEXT: [0xA4, 0x10, 0xA3, 0xB8]
// CHECK-NEXT: [0x07, 0x11, 0x66, 0xF8]
// CHECK-NEXT: [0xEA, 0x13, 0xE9, 0xB8]
