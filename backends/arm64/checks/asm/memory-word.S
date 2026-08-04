// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global memory_word
memory_word:
    ldr w0, [x1, 4]
    str w2, [sp, 8]

// CHECK: .section .text
// CHECK-NEXT: memory_word:
// CHECK-NEXT: [0x20, 0x04, 0x40, 0xB9]
// CHECK-NEXT: [0xE2, 0x0B, 0x00, 0xB9]
