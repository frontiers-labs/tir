# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# SSE2 scalar-double encodings: the F2 mandatory prefix, an optional REX byte
# (dropped for xmm0..xmm7 via the _norex forms; REX.R/REX.B add the 4th
# register-number bit for xmm8..xmm15), the 0x0F escape, the opcode, then
# ModR/M(mod=11, reg=dst, rm=src). The xmm0..xmm7 forms match GNU as exactly.

	.global fp
fp:
	addsd xmm0, xmm1
	subsd xmm2, xmm3
	mulsd xmm4, xmm5
	divsd xmm14, xmm15
	movsd xmm8, xmm0
	movq xmm9, r10
	cvtsi2sd xmm0, rax

# CHECK: fp:
# addsd xmm0, xmm1: opcode 58, ModR/M=11 reg(0) rm(1) = C1.
# CHECK-NEXT: [0xF2, 0x0F, 0x58, 0xC1]
# subsd xmm2, xmm3: opcode 5C, ModR/M=11 reg(2) rm(3) = D3.
# CHECK-NEXT: [0xF2, 0x0F, 0x5C, 0xD3]
# mulsd xmm4, xmm5: opcode 59, ModR/M=11 reg(4) rm(5) = E5.
# CHECK-NEXT: [0xF2, 0x0F, 0x59, 0xE5]
# divsd xmm14, xmm15: REX.RB=45, opcode 5E, ModR/M=11 reg(14&7=6) rm(15&7=7) = F7.
# CHECK-NEXT: [0xF2, 0x45, 0x0F, 0x5E, 0xF7]
# movsd xmm8, xmm0: REX.R=44, opcode 10, ModR/M=11 reg(8&7=0) rm(0) = C0.
# CHECK-NEXT: [0xF2, 0x44, 0x0F, 0x10, 0xC0]
# movq xmm9, r10: 66 prefix, REX.WRB=4D, opcode 6E, ModR/M=11 reg(1) rm(2) = CA.
# CHECK-NEXT: [0x66, 0x4D, 0x0F, 0x6E, 0xCA]
# cvtsi2sd xmm0, rax: F2 prefix, REX.W=48, opcode 2A, ModR/M C0.
# CHECK-NEXT: [0xF2, 0x48, 0x0F, 0x2A, 0xC0]
