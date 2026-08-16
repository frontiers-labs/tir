// RUN: tir mc --march=armv8.0-a --filetype=obj-ascii %s | filecheck %s

// Bitwise three-same operations, byte arrangements only.

.global advsimd_bitwise
advsimd_bitwise:
    and v0.8b, v9.8b, v18.8b
    and v1.16b, v10.16b, v19.16b
    orr v0.8b, v9.8b, v18.8b
    orr v1.16b, v10.16b, v19.16b
    eor v0.8b, v9.8b, v18.8b
    eor v1.16b, v10.16b, v19.16b
    bic v0.8b, v9.8b, v18.8b
    bic v1.16b, v10.16b, v19.16b
    orn v0.8b, v9.8b, v18.8b
    orn v1.16b, v10.16b, v19.16b

// CHECK: .section .text
// CHECK-NEXT: advsimd_bitwise:
// CHECK-NEXT: [0x20, 0x1D, 0x32, 0x0E]
// CHECK-NEXT: [0x41, 0x1D, 0x33, 0x4E]
// CHECK-NEXT: [0x20, 0x1D, 0xB2, 0x0E]
// CHECK-NEXT: [0x41, 0x1D, 0xB3, 0x4E]
// CHECK-NEXT: [0x20, 0x1D, 0x32, 0x2E]
// CHECK-NEXT: [0x41, 0x1D, 0x33, 0x6E]
// CHECK-NEXT: [0x20, 0x1D, 0x72, 0x0E]
// CHECK-NEXT: [0x41, 0x1D, 0x73, 0x4E]
// CHECK-NEXT: [0x20, 0x1D, 0xF2, 0x0E]
// CHECK-NEXT: [0x41, 0x1D, 0xF3, 0x4E]
