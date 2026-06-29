# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# 32-bit operand-size forms: same opcodes as the 64-bit encodings but with REX.W
# cleared, so the REX byte is 0x40 for the base GPRs (eax..edi) and 0x41/0x44/
# 0x45 once r8d..r15d set their 4th register-number bit in REX.B / REX.R.

	.global enc32
enc32:
	add eax, ebx
	mov eax, ebx
	sub esi, edi
	mov eax, 1
	add ecx, 42
	shl eax, 3
	add r8d, r9d
	mov r10d, r11d
	mov eax, r8d
	add r8d, eax
	add r8d, 42
	shl r9d, 3

# CHECK: enc32:
# add eax, ebx: REX.W=0 -> 40, opcode 01, ModR/M=11 reg(ebx=3) rm(eax=0) = D8.
# CHECK-NEXT: [0x40, 0x01, 0xD8]
# CHECK-NEXT: [0x40, 0x89, 0xD8]
# sub esi, edi: opcode 29, ModR/M=11 reg(edi=7) rm(esi=6) = FE.
# CHECK-NEXT: [0x40, 0x29, 0xFE]
# mov eax, 1: opcode C7, ModR/M=11 000 000 = C0, imm32=1.
# CHECK-NEXT: [0x40, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]
# add ecx, 42: opcode 81, ModR/M=11 000(/0) 001(ecx) = C1, imm32=42.
# CHECK-NEXT: [0x40, 0x81, 0xC1, 0x2A, 0x00, 0x00, 0x00]
# shl eax, 3: opcode C1, ModR/M=11 100(/4) 000(eax) = E0, imm8=3.
# CHECK-NEXT: [0x40, 0xC1, 0xE0, 0x03]
# add r8d, r9d: REX.RB=45, opcode 01, ModR/M=11 reg(r9d&7=1) rm(r8d&7=0) = C8.
# CHECK-NEXT: [0x45, 0x01, 0xC8]
# mov r10d, r11d: REX.RB=45, opcode 89, ModR/M=11 reg(r11d&7=3) rm(r10d&7=2) = DA.
# CHECK-NEXT: [0x45, 0x89, 0xDA]
# mov eax, r8d: REX.R=44 (src=r8d in reg), opcode 89, ModR/M=11 reg(0) rm(0) = C0.
# CHECK-NEXT: [0x44, 0x89, 0xC0]
# add r8d, eax: REX.B=41 (dst=r8d in rm), opcode 01, ModR/M=11 reg(0) rm(0) = C0.
# CHECK-NEXT: [0x41, 0x01, 0xC0]
# add r8d, 42: REX.B=41, opcode 81, ModR/M=11 000(/0) rm(r8d&7=0) = C0, imm32=42.
# CHECK-NEXT: [0x41, 0x81, 0xC0, 0x2A, 0x00, 0x00, 0x00]
# shl r9d, 3: REX.B=41, opcode C1, ModR/M=11 100(/4) rm(r9d&7=1) = E1, imm8=3.
# CHECK-NEXT: [0x41, 0xC1, 0xE1, 0x03]
