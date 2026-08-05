# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s
# RUN: tir mc --march=x86_64 --mattr=+sse2 --filetype=obj-ascii %s | filecheck %s
# RUN: not tir mc --march=x86_64 --mattr=-sse2 --filetype=obj-ascii %s
# RUN: not tir mc --march=x86_64 --mattr=-sse --filetype=obj-ascii %s
# RUN: not tir mc --march=x86_64 --mattr=-x86_64 --filetype=obj-ascii %s

.global scalar_float
scalar_float:
    addsd xmm0, xmm1

# CHECK: .section .text
# CHECK-NEXT: scalar_float:
# CHECK-NEXT: [0xF2, 0x0F, 0x58, 0xC1]
