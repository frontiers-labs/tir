// RUN: tir mc --march=armv8.0-a --filetype=obj-ascii %s | filecheck %s

// A conversion from a W register and one from an X register share a mnemonic;
// the register width picks the encoding.
.global float_convert
float_convert:
    scvtf d0, w1
    ucvtf d2, w3
    scvtf d4, x5
    ucvtf d6, x7
    scvtf s8, x9
    ucvtf s10, x11

// CHECK: .section .text
// CHECK-NEXT: float_convert:
// CHECK-NEXT: [0x20, 0x00, 0x62, 0x1E]
// CHECK-NEXT: [0x62, 0x00, 0x63, 0x1E]
// CHECK-NEXT: [0xA4, 0x00, 0x62, 0x9E]
// CHECK-NEXT: [0xE6, 0x00, 0x63, 0x9E]
// CHECK-NEXT: [0x28, 0x01, 0x22, 0x9E]
// CHECK-NEXT: [0x6A, 0x01, 0x23, 0x9E]
