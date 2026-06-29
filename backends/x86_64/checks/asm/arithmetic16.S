# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global alu16
alu16:
	add ax, bx
	sub cx, dx
	and si, di
	or bp, sp
	xor ax, ax
	mov bx, cx
	add ax, 42
	and cx, 255
	or dx, 1
	xor si, 7
	mov di, 100
	shl ax, 3
	shr bx, 1
	sar cx, 15
	add r8w, r9w
	mov r15w, r10w
	add r12w, 42

# CHECK:      asm.symbol {name = "alu16"}
# CHECK-NEXT: x86_64.add16 {dst = ax, src = bx}
# CHECK-NEXT: x86_64.sub16 {dst = cx, src = dx}
# CHECK-NEXT: x86_64.and16 {dst = si, src = di}
# CHECK-NEXT: x86_64.or16 {dst = bp, src = sp}
# CHECK-NEXT: x86_64.xor16 {dst = ax, src = ax}
# CHECK-NEXT: x86_64.mov16 {dst = bx, src = cx}
# CHECK-NEXT: x86_64.add_imm16 {dst = ax, imm = 42}
# CHECK-NEXT: x86_64.and_imm16 {dst = cx, imm = 255}
# CHECK-NEXT: x86_64.or_imm16 {dst = dx, imm = 1}
# CHECK-NEXT: x86_64.xor_imm16 {dst = si, imm = 7}
# CHECK-NEXT: x86_64.mov_imm16 {dst = di, imm = 100}
# CHECK-NEXT: x86_64.shl16 {dst = ax, imm = 3}
# CHECK-NEXT: x86_64.shr16 {dst = bx, imm = 1}
# CHECK-NEXT: x86_64.sar16 {dst = cx, imm = 15}
# CHECK-NEXT: x86_64.add16 {dst = r8w, src = r9w}
# CHECK-NEXT: x86_64.mov16 {dst = r15w, src = r10w}
# CHECK-NEXT: x86_64.add_imm16 {dst = r12w, imm = 42}
