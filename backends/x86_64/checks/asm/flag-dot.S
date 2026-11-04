# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global flag_setter
flag_setter:
	clc
	stc
	cmc
	cld
	std

# CHECK:      asm.symbol {name = "flag_setter"}
# CHECK-NEXT: x86_64.clc
# CHECK-NEXT: x86_64.stc
# CHECK-NEXT: x86_64.cmc
# CHECK-NEXT: x86_64.cld
# CHECK-NEXT: x86_64.std

# ENC:      [0xF8]
# ENC:      [0xF9]
# ENC:      [0xF5]
# ENC:      [0xFC]
# ENC:      [0xFD]
