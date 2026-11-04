# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global carrying_rotates
carrying_rotates:
	rcl rax, 3
	rcr rax, 3

# CHECK:      asm.symbol {name = "carrying_rotates"}
# CHECK-NEXT: x86_64.rcl {dst = rax, imm = 3}
# CHECK-NEXT: x86_64.rcr {dst = rax, imm = 3}

# ENC:      [0x48, 0xC1, 0xD0, 0x03]
# ENC:      [0x48, 0xC1, 0xD8, 0x03]
