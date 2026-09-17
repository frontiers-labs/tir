# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global counted_transfers
counted_transfers:
	loop 16
	loope 20
	loopne 24
	jrcxz 28

# CHECK:      asm.symbol {name = "counted_transfers"}
# CHECK-NEXT: x86_64.loop {imm = 16}
# CHECK-NEXT: x86_64.loope {imm = 20}
# CHECK-NEXT: x86_64.loopne {imm = 24}
# CHECK-NEXT: x86_64.jrcxz {imm = 28}

# ENC:      [0xE2, 0x10]
# ENC:      [0xE1, 0x14]
# ENC:      [0xE0, 0x18]
# ENC:      [0xE3, 0x1C]
