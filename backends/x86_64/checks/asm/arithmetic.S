# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global alu
alu:
	add rax, rbx
	sub rcx, rdx
	and rsi, rdi
	or rbp, rsp
	xor rax, rax
	mov rbx, rcx
	add rax, 42
	and rcx, 255
	or rdx, 1
	xor rsi, 7
	mov rdi, 100
	shl rax, 3
	shr rbx, 1
	sar rcx, 63

# CHECK:      asm.symbol {name = "alu"}
# CHECK-NEXT: x86_64.add {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.sub {dst = rcx, src = rdx}
# CHECK-NEXT: x86_64.and {dst = rsi, src = rdi}
# CHECK-NEXT: x86_64.or {dst = rbp, src = rsp}
# CHECK-NEXT: x86_64.xor {dst = rax, src = rax}
# CHECK-NEXT: x86_64.mov {dst = rbx, src = rcx}
# CHECK-NEXT: x86_64.add_imm {dst = rax, imm = 42}
# CHECK-NEXT: x86_64.and_imm {dst = rcx, imm = 255}
# CHECK-NEXT: x86_64.or_imm {dst = rdx, imm = 1}
# CHECK-NEXT: x86_64.xor_imm {dst = rsi, imm = 7}
# CHECK-NEXT: x86_64.mov_imm {dst = rdi, imm = 100}
# CHECK-NEXT: x86_64.shl {dst = rax, imm = 3}
# CHECK-NEXT: x86_64.shr {dst = rbx, imm = 1}
# CHECK-NEXT: x86_64.sar {dst = rcx, imm = 63}
