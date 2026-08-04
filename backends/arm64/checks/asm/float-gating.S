// RUN: tir mc --march=armv8-a --mattr=-advsimd --filetype=obj-ascii %s | filecheck %s
// RUN: not tir mc --march=armv8-a --mattr=-advsimd,-fp --filetype=obj-ascii %s

.global scalar_float
scalar_float:
    fadd d0, d1, d2

// CHECK: .section .text
// CHECK-NEXT: scalar_float:
// CHECK-NEXT: [0x20, 0x28, 0x62, 0x1E]
