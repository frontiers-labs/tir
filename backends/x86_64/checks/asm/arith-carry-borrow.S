# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global carry_sequence
carry_sequence:
	adc rax, rbx
	adc rcx, 4
	sbb rdx, rsi
	sbb r8, 4

# CHECK:      asm.symbol {name = "carry_sequence"}
# CHECK-NEXT: x86_64.adc {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.adc_imm {dst = rcx, imm = 4}
# CHECK-NEXT: x86_64.sbb {dst = rdx, src = rsi}
# CHECK-NEXT: x86_64.sbb_imm {dst = r8, imm = 4}

# ENC:      [0x48, 0x11, 0xD8]
# ENC:      [0x48, 0x83, 0xD1, 0x04]
# ENC:      [0x48, 0x19, 0xF2]
# ENC:      [0x49, 0x83, 0xD8, 0x04]
