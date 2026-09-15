# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

	.global vector_moves
vector_moves:
	movups xmm0, xmm1
	movups xmm8, xmm9
	movups xmm2, [rbx + 16]
	movups [rcx + 32], xmm3
	movaps xmm4, xmm5
	movaps xmm12, xmm13

# CHECK: vector_moves:
# CHECK-NEXT: [0x0F, 0x10, 0xC1]
# CHECK-NEXT: [0x45, 0x0F, 0x10, 0xC1]
# CHECK-NEXT: [0x0F, 0x10, 0x53, 0x10]
# CHECK-NEXT: [0x0F, 0x11, 0x59, 0x20]
# CHECK-NEXT: [0x0F, 0x28, 0xE5]
# CHECK-NEXT: [0x45, 0x0F, 0x28, 0xE5]
