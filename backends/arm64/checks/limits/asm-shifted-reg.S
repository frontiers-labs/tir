// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

// Shifted-register assembly must preserve the shift amount.

f:
    add x0, x1, x2, lsl 3

// CHECK: [0x20, 0x0C, 0x02, 0x8B]
