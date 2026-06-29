# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# 8-bit forms. The low-byte ALU opcode is one below the wider form (add 00 vs 01)
# and a REX byte is always emitted, which makes spl/bpl/sil/dil and r8b..r15b
# encodable. The high bytes ah/ch/dh/bh carry NO REX prefix, so their encoding is
# just opcode + ModR/M; their ModR/M numbers are 4..7.

	.global enc8
enc8:
	add al, bl
	mov al, bl
	sub sil, dil
	mov al, 1
	add cl, 42
	shl al, 3
	add r8b, r9b
	mov r10b, r11b
	add ah, bh
	mov ch, dh
	add ah, 42
	shl bh, 2

# CHECK: enc8:
# add al, bl: REX=40, opcode 00, ModR/M=11 reg(bl=3) rm(al=0) = D8.
# CHECK-NEXT: [0x40, 0x00, 0xD8]
# CHECK-NEXT: [0x40, 0x88, 0xD8]
# sub sil, dil: REX=40 enables the new low bytes, opcode 28, ModR/M=11 reg(dil=7) rm(sil=6) = FE.
# CHECK-NEXT: [0x40, 0x28, 0xFE]
# mov al, 1: opcode C6, ModR/M=11 000 000 = C0, imm8=1.
# CHECK-NEXT: [0x40, 0xC6, 0xC0, 0x01]
# add cl, 42: opcode 80, ModR/M=11 000(/0) 001(cl) = C1, imm8=42.
# CHECK-NEXT: [0x40, 0x80, 0xC1, 0x2A]
# shl al, 3: opcode C0, ModR/M=11 100(/4) 000(al) = E0, imm8=3.
# CHECK-NEXT: [0x40, 0xC0, 0xE0, 0x03]
# add r8b, r9b: REX.RB=45, opcode 00, ModR/M=11 reg(r9b&7=1) rm(r8b&7=0) = C8.
# CHECK-NEXT: [0x45, 0x00, 0xC8]
# mov r10b, r11b: REX.RB=45, opcode 88, ModR/M=11 reg(r11b&7=3) rm(r10b&7=2) = DA.
# CHECK-NEXT: [0x45, 0x88, 0xDA]
# add ah, bh: no REX, opcode 00, ModR/M=11 reg(bh=7) rm(ah=4) = FC.
# CHECK-NEXT: [0x00, 0xFC]
# mov ch, dh: no REX, opcode 88, ModR/M=11 reg(dh=6) rm(ch=5) = F5.
# CHECK-NEXT: [0x88, 0xF5]
# add ah, 42: no REX, opcode 80, ModR/M=11 000(/0) rm(ah=4) = C4, imm8=42.
# CHECK-NEXT: [0x80, 0xC4, 0x2A]
# shl bh, 2: no REX, opcode C0, ModR/M=11 100(/4) rm(bh=7) = E7, imm8=2.
# CHECK-NEXT: [0xC0, 0xE7, 0x02]
