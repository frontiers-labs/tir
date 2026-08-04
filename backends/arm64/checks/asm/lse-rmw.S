// RUN: tir mc --march=armv8.1-a --filetype=obj-ascii %s | filecheck %s

.global lse_rmw
lse_rmw:
    ldeor x0, x1, [x2]
    ldseta w3, w4, [x5]
    ldsmaxl x6, x7, [x8]
    ldsminal w9, w10, [sp]
    ldumax x11, x12, [x13]
    ldumina w14, w15, [x16]

// CHECK: .section .text
// CHECK-NEXT: lse_rmw:
// CHECK-NEXT: [0x41, 0x20, 0x20, 0xF8]
// CHECK-NEXT: [0xA4, 0x30, 0xA3, 0xB8]
// CHECK-NEXT: [0x07, 0x41, 0x66, 0xF8]
// CHECK-NEXT: [0xEA, 0x53, 0xE9, 0xB8]
// CHECK-NEXT: [0xAC, 0x61, 0x2B, 0xF8]
// CHECK-NEXT: [0x0F, 0x72, 0xAE, 0xB8]
