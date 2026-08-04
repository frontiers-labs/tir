// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global memory_word_register
memory_word_register:
    ldr w0, [x1, x2]
    str w3, [sp, x4]
    ldr w5, [x6, x7, lsl 2]
    str w8, [x9, x10, lsl 2]

// CHECK: .section .text
// CHECK-NEXT: memory_word_register:
// CHECK-NEXT: [0x20, 0x68, 0x62, 0xB8]
// CHECK-NEXT: [0xE3, 0x6B, 0x24, 0xB8]
// CHECK-NEXT: [0xC5, 0x78, 0x67, 0xB8]
// CHECK-NEXT: [0x28, 0x79, 0x2A, 0xB8]
