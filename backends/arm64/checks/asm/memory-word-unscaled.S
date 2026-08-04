// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global memory_word_unscaled
memory_word_unscaled:
    ldur w0, [x1, -4]
    stur w2, [sp, 12]

// CHECK: .section .text
// CHECK-NEXT: memory_word_unscaled:
// CHECK-NEXT: [0x20, 0xC0, 0x5F, 0xB8]
// CHECK-NEXT: [0xE2, 0xC3, 0x00, 0xB8]
