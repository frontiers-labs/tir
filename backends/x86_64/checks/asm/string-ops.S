# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global string_transfers
string_transfers:
	movsb
	movsd
	movsq
	stosb
	stosq
	cmpsb
	cmpsq

# CHECK:      asm.symbol {name = "string_transfers"}
# CHECK-NEXT: x86_64.movsb
# CHECK-NEXT: x86_64.movsd_string
# CHECK-NEXT: x86_64.movsq
# CHECK-NEXT: x86_64.stosb
# CHECK-NEXT: x86_64.stosq
# CHECK-NEXT: x86_64.cmpsb
# CHECK-NEXT: x86_64.cmpsq

# ENC:      [0xA4]
# ENC:      [0xA5]
# ENC:      [0x48, 0xA5]
# ENC:      [0xAA]
# ENC:      [0x48, 0xAB]
# ENC:      [0xA6]
# ENC:      [0x48, 0xA7]
