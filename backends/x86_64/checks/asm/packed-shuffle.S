# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global lane_shuffles
lane_shuffles:
	shufps xmm0, xmm1, 0x1b
	shufpd xmm0, xmm1, 0x1

# CHECK:      asm.symbol {name = "lane_shuffles"}
# CHECK-NEXT: x86_64.shufps {dst = xmm0, src = xmm1, imm = 27}
# CHECK-NEXT: x86_64.shufpd {dst = xmm0, src = xmm1, imm = 1}

# ENC:      [0x0F, 0xC6, 0xC1, 0x1B]
# ENC:      [0x66, 0x0F, 0xC6, 0xC1, 0x01]
