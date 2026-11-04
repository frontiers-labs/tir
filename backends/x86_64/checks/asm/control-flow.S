# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global cf
cf:
	jmp 16
	call 256
	cmp rax, rcx
	test rdx, rdx
	je 16
	jne 16
	jl 16
	jge 16
	jb 16
	jae 16
	ret

# CHECK:      asm.symbol {name = "cf"}
# CHECK-NEXT: x86_64.jmp {imm = 16}
# CHECK-NEXT: x86_64.call {imm = 256}
# CHECK-NEXT: x86_64.cmp {dst = rax, src = rcx}
# CHECK-NEXT: x86_64.test {dst = rdx, src = rdx}
# CHECK-NEXT: x86_64.je {imm = 16}
# CHECK-NEXT: x86_64.jne {imm = 16}
# CHECK-NEXT: x86_64.jl {imm = 16}
# CHECK-NEXT: x86_64.jge {imm = 16}
# CHECK-NEXT: x86_64.jb {imm = 16}
# CHECK-NEXT: x86_64.jae {imm = 16}
# CHECK-NEXT: x86_64.ret
