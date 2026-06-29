# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# Fixed-length REX.W encodings: opcode byte(s) + ModR/M (+ immediate). The base
# GPRs use the 3-bit ModR/M field; r8..r15 set their 4th number bit in REX.R
# (reg field) or REX.B (rm field), so REX changes from 0x48 to 0x49/0x4C/0x4D.

	.global enc
enc:
	add rax, rbx
	mov rax, rbx
	sub rsi, rdi
	mov rax, 1
	add rcx, 42
	shl rax, 3
	add r8, r9
	mov r10, r11
	mov rax, r8
	add r8, rax
	add r8, 42
	shl r9, 3
	jmp 16
	call 256
	ret

# CHECK: enc:
# add rax, rbx: REX.W=48, opcode 01, ModR/M=11 reg(rbx=3) rm(rax=0) = D8.
# CHECK-NEXT: [0x48, 0x01, 0xD8]
# CHECK-NEXT: [0x48, 0x89, 0xD8]
# sub rsi, rdi: opcode 29, ModR/M=11 reg(rdi=7) rm(rsi=6) = FE.
# CHECK-NEXT: [0x48, 0x29, 0xFE]
# mov rax, 1: opcode C7, ModR/M=11 000 000 = C0, imm32=1.
# CHECK-NEXT: [0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]
# add rcx, 42: opcode 81, ModR/M=11 000(/0) 001(rcx) = C1, imm32=42.
# CHECK-NEXT: [0x48, 0x81, 0xC1, 0x2A, 0x00, 0x00, 0x00]
# shl rax, 3: opcode C1, ModR/M=11 100(/4) 000(rax) = E0, imm8=3.
# CHECK-NEXT: [0x48, 0xC1, 0xE0, 0x03]
# add r8, r9: REX.WRB=4D, opcode 01, ModR/M=11 reg(r9&7=1) rm(r8&7=0) = C8.
# CHECK-NEXT: [0x4D, 0x01, 0xC8]
# mov r10, r11: REX.WRB=4D, opcode 89, ModR/M=11 reg(r11&7=3) rm(r10&7=2) = DA.
# CHECK-NEXT: [0x4D, 0x89, 0xDA]
# mov rax, r8: REX.WR=4C (src=r8 in reg), opcode 89, ModR/M=11 reg(0) rm(0) = C0.
# CHECK-NEXT: [0x4C, 0x89, 0xC0]
# add r8, rax: REX.WB=49 (dst=r8 in rm), opcode 01, ModR/M=11 reg(0) rm(0) = C0.
# CHECK-NEXT: [0x49, 0x01, 0xC0]
# add r8, 42: REX.WB=49, opcode 81, ModR/M=11 000(/0) rm(r8&7=0) = C0, imm32=42.
# CHECK-NEXT: [0x49, 0x81, 0xC0, 0x2A, 0x00, 0x00, 0x00]
# shl r9, 3: REX.WB=49, opcode C1, ModR/M=11 100(/4) rm(r9&7=1) = E1, imm8=3.
# CHECK-NEXT: [0x49, 0xC1, 0xE1, 0x03]
# jmp rel32 / call rel32 / ret.
# CHECK-NEXT: [0xE9, 0x10, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0xE8, 0x00, 0x01, 0x00, 0x00]
# CHECK-NEXT: [0xC3]
