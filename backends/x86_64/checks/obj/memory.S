# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# Memory operands. `[base]` uses ModR/M mod=00 with the base register in the rm
# field; only bases whose low three bits avoid rsp(100)/rbp(101) are valid in
# this form. Indirect `*target` uses 0xFF with mod=11; the REX prefix carries
# only REX.B (W=0), so a base register adds a redundant 0x40 prefix.

	.global mem
mem:
	mov rax, [rbx]
	mov [rcx], rdx
	mov r9, [r10]
	mov [r11], r12
	jmp *rsi
	call *r8

# CHECK: mem:
# mov rax, [rbx]: REX.W=48, opcode 8B, ModR/M=00 reg(rax=0) rm(rbx=3) = 03.
# CHECK-NEXT: [0x48, 0x8B, 0x03]
# mov [rcx], rdx: opcode 89, ModR/M=00 reg(rdx=2) rm(rcx=1) = 11.
# CHECK-NEXT: [0x48, 0x89, 0x11]
# mov r9, [r10]: REX.WRB=4D, ModR/M=00 reg(r9&7=1) rm(r10&7=2) = 0A.
# CHECK-NEXT: [0x4D, 0x8B, 0x0A]
# mov [r11], r12: REX.WRB=4D, opcode 89, ModR/M=00 reg(r12&7=4) rm(r11&7=3) = 23.
# CHECK-NEXT: [0x4D, 0x89, 0x23]
# jmp *rsi: REX=40, opcode FF, ModR/M=11 reg(/4=100) rm(rsi=6) = E6.
# CHECK-NEXT: [0x40, 0xFF, 0xE6]
# call *r8: REX.B=41, opcode FF, ModR/M=11 reg(/2=010) rm(r8&7=0) = D0.
# CHECK-NEXT: [0x41, 0xFF, 0xD0]
