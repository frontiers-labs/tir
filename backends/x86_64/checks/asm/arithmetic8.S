# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

# Low byte (al..r15b) and high byte (ah/ch/dh/bh) share the mnemonics; the
# operand register class selects the form.

	.global alu8
alu8:
	add al, bl
	sub cl, dl
	and sil, dil
	or bpl, spl
	xor al, al
	mov bl, cl
	add al, 42
	and cl, 255
	or dl, 1
	xor sil, 7
	mov dil, 100
	shl al, 3
	shr bl, 1
	sar cl, 7
	add r8b, r9b
	mov r15b, r10b
	add r12b, 42
	add ah, bh
	mov ch, dh
	xor ah, ah
	add ah, 42
	shl bh, 2

# CHECK:      asm.symbol {name = "alu8"}
# CHECK-NEXT: x86_64.add8 {dst = al, src = bl}
# CHECK-NEXT: x86_64.sub8 {dst = cl, src = dl}
# CHECK-NEXT: x86_64.and8 {dst = sil, src = dil}
# CHECK-NEXT: x86_64.or8 {dst = bpl, src = spl}
# CHECK-NEXT: x86_64.xor8 {dst = al, src = al}
# CHECK-NEXT: x86_64.mov8 {dst = bl, src = cl}
# CHECK-NEXT: x86_64.add_imm8 {dst = al, imm = 42}
# CHECK-NEXT: x86_64.and_imm8 {dst = cl, imm = 255}
# CHECK-NEXT: x86_64.or_imm8 {dst = dl, imm = 1}
# CHECK-NEXT: x86_64.xor_imm8 {dst = sil, imm = 7}
# CHECK-NEXT: x86_64.mov_imm8 {dst = dil, imm = 100}
# CHECK-NEXT: x86_64.shl8 {dst = al, imm = 3}
# CHECK-NEXT: x86_64.shr8 {dst = bl, imm = 1}
# CHECK-NEXT: x86_64.sar8 {dst = cl, imm = 7}
# CHECK-NEXT: x86_64.add8 {dst = r8b, src = r9b}
# CHECK-NEXT: x86_64.mov8 {dst = r15b, src = r10b}
# CHECK-NEXT: x86_64.add_imm8 {dst = r12b, imm = 42}
# CHECK-NEXT: x86_64.add8h {dst = ah, src = bh}
# CHECK-NEXT: x86_64.mov8h {dst = ch, src = dh}
# CHECK-NEXT: x86_64.xor8h {dst = ah, src = ah}
# CHECK-NEXT: x86_64.add_imm8h {dst = ah, imm = 42}
# CHECK-NEXT: x86_64.shl8h {dst = bh, imm = 2}
