// RUN: tir mc --march=armv8.1-a --filetype=obj-ascii %s | filecheck %s
// RUN: tir mc --march=armv8-a --mattr=+lse --filetype=obj-ascii %s | filecheck %s
// RUN: not tir mc --march=armv8-a --filetype=obj-ascii %s

.global lse_atomics
lse_atomics:
    swp x0, x1, [x2]
    swpa w3, w4, [x5]
    swpl x6, x7, [x8]
    swpal w9, w10, [sp]
    ldadd x11, x12, [x13]
    ldadda w14, w15, [x16]
    ldaddl x17, x18, [x19]
    ldaddal w20, w21, [x22]

// CHECK: .section .text
// CHECK-NEXT: lse_atomics:
// CHECK-NEXT: [0x41, 0x80, 0x20, 0xF8]
// CHECK-NEXT: [0xA4, 0x80, 0xA3, 0xB8]
// CHECK-NEXT: [0x07, 0x81, 0x66, 0xF8]
// CHECK-NEXT: [0xEA, 0x83, 0xE9, 0xB8]
// CHECK-NEXT: [0xAC, 0x01, 0x2B, 0xF8]
// CHECK-NEXT: [0x0F, 0x02, 0xAE, 0xB8]
// CHECK-NEXT: [0x72, 0x02, 0x71, 0xF8]
// CHECK-NEXT: [0xD5, 0x02, 0xF4, 0xB8]
