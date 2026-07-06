# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

	.global enc_ext
enc_ext:
	neg rax
	not rcx
	sub rax, 42
	cmp rbx, 7
	test rdx, 5
	imul rbx
	shl rax, cl
	shr r8, cl
	rol rax, 3
	ror rbx, 2
	movzx rax, bl
	movzx rcx, dx
	movsx rax, bl
	movsx rcx, dx
	movsxd rax, ebx
	movabs rax, 81985529216486895
	movabs r9, 2
	sete al
	setl dil
	seta r9b
	cmove rax, rbx
	cmovg r8, r9
	jle 16
	jo 16
	movzx rax, byte ptr [rbx]
	movsx rcx, word ptr [rdx]
	movsxd rax, dword ptr [rbx]
	mov eax, [rbx]
	mov [rbx], eax
	mov [rbx], cx
	mov [rbx], dl
	mov rax, [rbx + 8]
	mov [rcx + 16], rdx
	push rbx
	push r12
	pop rbx
	pop r12
	neg eax
	sub eax, 42
	cmp ebx, ecx
	test edx, 5
	shl eax, cl
	rol eax, 3

# CHECK: enc_ext:
# neg rax: REX.W F7 /3, ModR/M=11 011 000 = D8; not rcx: /2 = D1.
# CHECK-NEXT: [0x48, 0xF7, 0xD8]
# CHECK-NEXT: [0x48, 0xF7, 0xD1]
# sub rax, 42: 81 /5 = E8; cmp rbx, 7: 81 /7 = FB; test rdx, 5: F7 /0 = C2.
# CHECK-NEXT: [0x48, 0x83, 0xE8, 0x2A]
# CHECK-NEXT: [0x48, 0x83, 0xFB, 0x07]
# CHECK-NEXT: [0x48, 0xF7, 0xC2, 0x05, 0x00, 0x00, 0x00]
# imul rbx: F7 /5 = EB (rdx:rax = rax * rbx).
# CHECK-NEXT: [0x48, 0xF7, 0xEB]
# cl-count shifts: D3 /4 rax = E0; /5 on r8 sets REX.B.
# CHECK-NEXT: [0x48, 0xD3, 0xE0]
# CHECK-NEXT: [0x49, 0xD3, 0xE8]
# rotates: C1 /0 = C0, /1 = CB, then imm8.
# CHECK-NEXT: [0x48, 0xC1, 0xC0, 0x03]
# CHECK-NEXT: [0x48, 0xC1, 0xCB, 0x02]
# movzx/movsx r64, r8/r16: 0F B6/B7/BE/BF, dst in reg, src in rm.
# CHECK-NEXT: [0x48, 0x0F, 0xB6, 0xC3]
# CHECK-NEXT: [0x48, 0x0F, 0xB7, 0xCA]
# CHECK-NEXT: [0x48, 0x0F, 0xBE, 0xC3]
# CHECK-NEXT: [0x48, 0x0F, 0xBF, 0xCA]
# movsxd rax, ebx: REX.W 63 /r.
# CHECK-NEXT: [0x48, 0x63, 0xC3]
# movabs: REX.W B8+rd, imm64 (0x0123456789ABCDEF little-endian).
# CHECK-NEXT: [0x48, 0xB8, 0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]
# CHECK-NEXT: [0x49, 0xB9, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
# setcc r8: 0F 90|cc, dst in rm (REX always, as for all GPR8 forms).
# CHECK-NEXT: [0x0F, 0x94, 0xC0]
# CHECK-NEXT: [0x40, 0x0F, 0x9C, 0xC7]
# CHECK-NEXT: [0x41, 0x0F, 0x97, 0xC1]
# cmovcc r64, r64: 0F 40|cc, dst in reg.
# CHECK-NEXT: [0x48, 0x0F, 0x44, 0xC3]
# CHECK-NEXT: [0x4D, 0x0F, 0x4F, 0xC1]
# jle/jo rel32.
# CHECK-NEXT: [0x0F, 0x8E, 0x10, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0x0F, 0x80, 0x10, 0x00, 0x00, 0x00]
# extending loads: mod=00, reg=dst, rm=base.
# CHECK-NEXT: [0x48, 0x0F, 0xB6, 0x03]
# CHECK-NEXT: [0x48, 0x0F, 0xBF, 0x0A]
# CHECK-NEXT: [0x48, 0x63, 0x03]
# narrow mov loads/stores (REX always, as for all GPR32/GPR16/GPR8 forms).
# CHECK-NEXT: [0x40, 0x8B, 0x03]
# CHECK-NEXT: [0x40, 0x89, 0x03]
# CHECK-NEXT: [0x66, 0x40, 0x89, 0x0B]
# CHECK-NEXT: [0x40, 0x88, 0x13]
# disp32 forms: mod=10 + disp32. Divergence: TIR always uses disp32 for a
# non-zero displacement; GNU as would pick the shorter disp8 (mod=01) form
# (48 8b 43 08 / 48 89 51 10). Both valid; TIR's is 3 bytes longer.
# CHECK-NEXT: [0x48, 0x8B, 0x83, 0x08, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0x48, 0x89, 0x91, 0x10, 0x00, 0x00, 0x00]
# push/pop: 50+rd / 58+rd behind a plain REX.
# CHECK-NEXT: [0x53]
# CHECK-NEXT: [0x41, 0x54]
# CHECK-NEXT: [0x5B]
# CHECK-NEXT: [0x41, 0x5C]
# 32-bit forms drop REX.W, and neg/not (F7), cmp/test reg-reg (39/85) and the
# by-cl and rotate shifts (D3/C1) now drop the empty REX entirely for a low
# register via their _norex canonicalization, matching GNU as
# (f7 d8 / 39 cb / d3 e0 / c1 c0 03).
# CHECK-NEXT: [0xF7, 0xD8]
# CHECK-NEXT: [0x83, 0xE8, 0x2A]
# CHECK-NEXT: [0x39, 0xCB]
# CHECK-NEXT: [0xF7, 0xC2, 0x05, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0xD3, 0xE0]
# CHECK-NEXT: [0xC1, 0xC0, 0x03]
