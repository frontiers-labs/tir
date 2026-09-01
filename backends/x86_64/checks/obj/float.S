# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# Scalar SSE/SSE2 encodings use a mandatory prefix where required, followed by
# an optional REX byte, the 0x0F escape, the opcode, and ModR/M. Low-register
# forms omit REX; REX.R/REX.B add the fourth register-number bit.

	.global fp
fp:
	addss xmm0, xmm1
	divss xmm14, xmm15
	movss xmm1, [rax + 8]
	movss [rax + 8], xmm1
	movss xmm2, [rsp + 8]
	movss xmm9, [r12 + 8]
	movss [r12 + 8], xmm9
	cvtsi2ss xmm0, eax
	cvtsi2ss xmm0, rax
	cvttss2si eax, xmm0
	cvttss2si rax, xmm0
	ucomiss xmm0, xmm1
	addsd xmm0, xmm1
	subsd xmm2, xmm3
	mulsd xmm4, xmm5
	divsd xmm14, xmm15
	movsd xmm8, xmm0
	movsd xmm1, [rax + 8]
	movsd [rax + 8], xmm1
	movq xmm9, r10
	movq rax, xmm0
	movd xmm0, eax
	movd eax, xmm1
	movd xmm9, r10d
	movd r10d, xmm9
	cvtsi2sd xmm0, eax
	cvtsi2sd xmm0, rax
	cvttsd2si eax, xmm0
	cvttsd2si rax, xmm0
	ucomisd xmm0, xmm1
	setp al
	setnp al

# CHECK: fp:
# addss xmm0, xmm1: F3 prefix, opcode 58, ModR/M C1.
# CHECK-NEXT: [0xF3, 0x0F, 0x58, 0xC1]
# divss xmm14, xmm15: F3 prefix, REX.RB=45, opcode 5E, ModR/M F7.
# CHECK-NEXT: [0xF3, 0x45, 0x0F, 0x5E, 0xF7]
# movss xmm1, [rax + 8]: F3 prefix, opcode 10, ModR/M 88, disp32.
# CHECK-NEXT: [0xF3, 0x0F, 0x10, 0x48, 0x08]
# movss [rax + 8], xmm1: opcode 11 with the same ModR/M and displacement.
# CHECK-NEXT: [0xF3, 0x0F, 0x11, 0x48, 0x08]
# movss xmm2, [rsp + 8]: low SIB form, ModR/M 94, SIB 24, disp32.
# CHECK-NEXT: [0xF3, 0x0F, 0x10, 0x54, 0x24, 0x08]
# movss xmm9, [r12 + 8]: REX.RB=45, ModR/M 8C, SIB 24, disp32.
# CHECK-NEXT: [0xF3, 0x45, 0x0F, 0x10, 0x4C, 0x24, 0x08]
# movss [r12 + 8], xmm9: opcode 11 with the same REX, ModR/M, SIB, and displacement.
# CHECK-NEXT: [0xF3, 0x45, 0x0F, 0x11, 0x4C, 0x24, 0x08]
# cvtsi2ss xmm0, eax: F3 prefix, opcode 2A, ModR/M C0.
# CHECK-NEXT: [0xF3, 0x0F, 0x2A, 0xC0]
# cvtsi2ss xmm0, rax: REX.W selects the signed 64-bit source.
# CHECK-NEXT: [0xF3, 0x48, 0x0F, 0x2A, 0xC0]
# cvttss2si eax, xmm0: F3 prefix, opcode 2C, ModR/M C0.
# CHECK-NEXT: [0xF3, 0x0F, 0x2C, 0xC0]
# cvttss2si rax, xmm0: REX.W selects the signed 64-bit result.
# CHECK-NEXT: [0xF3, 0x48, 0x0F, 0x2C, 0xC0]
# ucomiss xmm0, xmm1: opcode 2E, ModR/M C1.
# CHECK-NEXT: [0x0F, 0x2E, 0xC1]
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
# movsd xmm1, [rax + 8]: low load omits REX.
# CHECK-NEXT: [0xF2, 0x0F, 0x10, 0x48, 0x08]
# movsd [rax + 8], xmm1: low store omits REX.
# CHECK-NEXT: [0xF2, 0x0F, 0x11, 0x48, 0x08]
# movq xmm9, r10: 66 prefix, REX.WRB=4D, opcode 6E, ModR/M=11 reg(1) rm(2) = CA.
# CHECK-NEXT: [0x66, 0x4D, 0x0F, 0x6E, 0xCA]
# movq rax, xmm0: 66 prefix, REX.W=48, opcode 7E, ModR/M=C0.
# CHECK-NEXT: [0x66, 0x48, 0x0F, 0x7E, 0xC0]
# movd low-register forms omit REX in both directions.
# CHECK-NEXT: [0x66, 0x0F, 0x6E, 0xC0]
# CHECK-NEXT: [0x66, 0x0F, 0x7E, 0xC8]
# movd high-register forms carry REX.RB without REX.W.
# CHECK-NEXT: [0x66, 0x45, 0x0F, 0x6E, 0xCA]
# CHECK-NEXT: [0x66, 0x45, 0x0F, 0x7E, 0xCA]
# cvtsi2sd xmm0, eax: F2 prefix, opcode 2A, ModR/M C0.
# CHECK-NEXT: [0xF2, 0x0F, 0x2A, 0xC0]
# cvtsi2sd xmm0, rax: F2 prefix, REX.W=48, opcode 2A, ModR/M C0.
# CHECK-NEXT: [0xF2, 0x48, 0x0F, 0x2A, 0xC0]
# cvttsd2si eax, xmm0: F2 prefix, opcode 2C, ModR/M C0.
# CHECK-NEXT: [0xF2, 0x0F, 0x2C, 0xC0]
# cvttsd2si rax, xmm0: F2 prefix, REX.W=48, opcode 2C, ModR/M C0.
# CHECK-NEXT: [0xF2, 0x48, 0x0F, 0x2C, 0xC0]
# ucomisd xmm0, xmm1: 66 prefix, opcode 2E, ModR/M C1.
# CHECK-NEXT: [0x66, 0x0F, 0x2E, 0xC1]
# CHECK-NEXT: [0x0F, 0x9A, 0xC0]
# CHECK-NEXT: [0x0F, 0x9B, 0xC0]
