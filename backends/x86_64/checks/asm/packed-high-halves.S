# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global high_halves
high_halves:
	movhps xmm0, [rax + 24]
	movhpd xmm0, [rax + 24]
	movhps [rax + 64], xmm0
	movhpd [rax + 64], xmm0

# CHECK:      asm.symbol {name = "high_halves"}
# CHECK-NEXT: x86_64.movhps_load_disp {dst = xmm0, base = rax, imm = 24}
# CHECK-NEXT: x86_64.movhpd_load_disp {dst = xmm0, base = rax, imm = 24}
# CHECK-NEXT: x86_64.movhps_store_disp {base = rax, src = xmm0, imm = 64}
# CHECK-NEXT: x86_64.movhpd_store_disp {base = rax, src = xmm0, imm = 64}

# ENC:      [0x0F, 0x16, 0x40, 0x18]
# ENC:      [0x66, 0x0F, 0x16, 0x40, 0x18]
# ENC:      [0x0F, 0x17, 0x40, 0x40]
# ENC:      [0x66, 0x0F, 0x17, 0x40, 0x40]
