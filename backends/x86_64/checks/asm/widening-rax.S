# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global sign_widening
sign_widening:
	cdqe
	cbw
	cwde
	cwd

# CHECK:      asm.symbol {name = "sign_widening"}
# CHECK-NEXT: x86_64.cdqe
# CHECK-NEXT: x86_64.cbw
# CHECK-NEXT: x86_64.cwde
# CHECK-NEXT: x86_64.cwd

# ENC:      [0x48, 0x98]
# ENC:      [0x66, 0x98]
# ENC:      [0x98]
# ENC:      [0x66, 0x99]
