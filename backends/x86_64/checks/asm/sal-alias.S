# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global arithmetic_shift_left
arithmetic_shift_left:
	sal rcx, cl

# CHECK:      asm.symbol {name = "arithmetic_shift_left"}
# CHECK-NEXT: x86_64.sal_cl {dst = rcx}

# ENC:      [0x48, 0xD3, 0xE1]
