# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global wide_multiply
wide_multiply:
	mul rcx

# CHECK:      asm.symbol {name = "wide_multiply"}
# CHECK-NEXT: x86_64.mul_wide {dst = rcx}

# ENC:      [0x48, 0xF7, 0xE1]
