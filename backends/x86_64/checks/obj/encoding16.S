# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# 16-bit operand-size forms: the 0x66 prefix precedes the 32-bit encoding, so the
# REX byte (0x40 for the base GPRs, 0x45 once r8w..r15w set REX.R/REX.B) and the
# rest follow it. Immediates shrink to 16 bits.

	.global enc16
enc16:
	add ax, bx
	mov ax, bx
	sub si, di
	mov ax, 1
	add cx, 42
	shl ax, 3
	add r8w, r9w
	mov r10w, r11w

# CHECK: enc16:
# add ax, bx: 66 prefix, REX.W=0 -> 40, opcode 01, ModR/M=11 reg(bx=3) rm(ax=0) = D8.
# CHECK-NEXT: [0x66, 0x40, 0x01, 0xD8]
# CHECK-NEXT: [0x66, 0x40, 0x89, 0xD8]
# sub si, di: opcode 29, ModR/M=11 reg(di=7) rm(si=6) = FE.
# CHECK-NEXT: [0x66, 0x40, 0x29, 0xFE]
# mov ax, 1: opcode C7, ModR/M=11 000 000 = C0, imm16=1.
# CHECK-NEXT: [0x66, 0x40, 0xC7, 0xC0, 0x01, 0x00]
# add cx, 42: opcode 81, ModR/M=11 000(/0) 001(cx) = C1, imm16=42.
# CHECK-NEXT: [0x66, 0x40, 0x81, 0xC1, 0x2A, 0x00]
# shl ax, 3: opcode C1, ModR/M=11 100(/4) 000(ax) = E0, imm8=3.
# CHECK-NEXT: [0x66, 0x40, 0xC1, 0xE0, 0x03]
# add r8w, r9w: REX.RB=45, opcode 01, ModR/M=11 reg(r9w&7=1) rm(r8w&7=0) = C8.
# CHECK-NEXT: [0x66, 0x45, 0x01, 0xC8]
# mov r10w, r11w: REX.RB=45, opcode 89, ModR/M=11 reg(r11w&7=3) rm(r10w&7=2) = DA.
# CHECK-NEXT: [0x66, 0x45, 0x89, 0xDA]
