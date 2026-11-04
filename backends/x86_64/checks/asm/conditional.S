# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global conditional
conditional:
	jle 16
	jg 16
	jbe 16
	ja 16
	js 16
	jns 16
	jo 16
	jno 16
	sete al
	setne bl
	setl dil
	setge spl
	setle r9b
	setg r10b
	setb cl
	setae dl
	setbe sil
	seta r15b
	cmove rax, rbx
	cmovne rcx, rdx
	cmovl rsi, rdi
	cmovge r8, r9
	cmovle r10, r11
	cmovg r12, r13
	cmovb r14, r15
	cmovae rax, r8
	cmovbe rbx, r9
	cmova rcx, r10

# CHECK:      asm.symbol {name = "conditional"}
# CHECK-NEXT: x86_64.jle {imm = 16}
# CHECK-NEXT: x86_64.jg {imm = 16}
# CHECK-NEXT: x86_64.jbe {imm = 16}
# CHECK-NEXT: x86_64.ja {imm = 16}
# CHECK-NEXT: x86_64.js {imm = 16}
# CHECK-NEXT: x86_64.jns {imm = 16}
# CHECK-NEXT: x86_64.jo {imm = 16}
# CHECK-NEXT: x86_64.jno {imm = 16}
# CHECK-NEXT: x86_64.sete {dst = al}
# CHECK-NEXT: x86_64.setne {dst = bl}
# CHECK-NEXT: x86_64.setl {dst = dil}
# CHECK-NEXT: x86_64.setge {dst = spl}
# CHECK-NEXT: x86_64.setle {dst = r9b}
# CHECK-NEXT: x86_64.setg {dst = r10b}
# CHECK-NEXT: x86_64.setb {dst = cl}
# CHECK-NEXT: x86_64.setae {dst = dl}
# CHECK-NEXT: x86_64.setbe {dst = sil}
# CHECK-NEXT: x86_64.seta {dst = r15b}
# CHECK-NEXT: x86_64.cmove {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.cmovne {dst = rcx, src = rdx}
# CHECK-NEXT: x86_64.cmovl {dst = rsi, src = rdi}
# CHECK-NEXT: x86_64.cmovge {dst = r8, src = r9}
# CHECK-NEXT: x86_64.cmovle {dst = r10, src = r11}
# CHECK-NEXT: x86_64.cmovg {dst = r12, src = r13}
# CHECK-NEXT: x86_64.cmovb {dst = r14, src = r15}
# CHECK-NEXT: x86_64.cmovae {dst = rax, src = r8}
# CHECK-NEXT: x86_64.cmovbe {dst = rbx, src = r9}
# CHECK-NEXT: x86_64.cmova {dst = rcx, src = r10}
