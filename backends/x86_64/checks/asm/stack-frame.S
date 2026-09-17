# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global stack_frame
stack_frame:
	nop
	pushf
	leave
	popf

# CHECK:      asm.symbol {name = "stack_frame"}
# CHECK-NEXT: x86_64.nop
# CHECK-NEXT: x86_64.pushf
# CHECK-NEXT: x86_64.leave
# CHECK-NEXT: x86_64.popf

# ENC:      [0x90]
# ENC:      [0x9C]
# ENC:      [0xC9]
# ENC:      [0x9D]
