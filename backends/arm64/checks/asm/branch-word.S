// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global branch_word
branch_word:
    cbz w0, 2
    cbnz w1, 2
    tbz w2, 3, 2
    tbnz w3, 31, 2

// CHECK: .section .text
// CHECK-NEXT: branch_word:
// CHECK-NEXT: [0x40, 0x00, 0x00, 0x34]
// CHECK-NEXT: [0x41, 0x00, 0x00, 0x35]
// CHECK-NEXT: [0x42, 0x00, 0x18, 0x36]
// CHECK-NEXT: [0x43, 0x00, 0xF8, 0x37]
