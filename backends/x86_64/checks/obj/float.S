# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# SSE2 scalar-double encodings: the F2 mandatory prefix, a REX byte (0x40 for
# xmm0..xmm7; REX.R/REX.B add the 4th register-number bit for xmm8..xmm15), the
# 0x0F escape, the opcode, then ModR/M(mod=11, reg=dst, rm=src).

	.global fp
fp:
	addsd xmm0, xmm1
	subsd xmm2, xmm3
	mulsd xmm4, xmm5
	divsd xmm14, xmm15
	movsd xmm8, xmm0

# CHECK: fp:
# addsd xmm0, xmm1: opcode 58, ModR/M=11 reg(0) rm(1) = C1.
# CHECK-NEXT: [0xF2, 0x40, 0x0F, 0x58, 0xC1]
# subsd xmm2, xmm3: opcode 5C, ModR/M=11 reg(2) rm(3) = D3.
# CHECK-NEXT: [0xF2, 0x40, 0x0F, 0x5C, 0xD3]
# mulsd xmm4, xmm5: opcode 59, ModR/M=11 reg(4) rm(5) = E5.
# CHECK-NEXT: [0xF2, 0x40, 0x0F, 0x59, 0xE5]
# divsd xmm14, xmm15: REX.RB=45, opcode 5E, ModR/M=11 reg(14&7=6) rm(15&7=7) = F7.
# CHECK-NEXT: [0xF2, 0x45, 0x0F, 0x5E, 0xF7]
# movsd xmm8, xmm0: REX.R=44, opcode 10, ModR/M=11 reg(8&7=0) rm(0) = C0.
# CHECK-NEXT: [0xF2, 0x44, 0x0F, 0x10, 0xC0]
