# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global cf
cf:
	jmp 16
	call 256
	ret

# CHECK:      asm.symbol {name = "cf"}
# CHECK-NEXT: x86_64.jmp {imm = 16}
# CHECK-NEXT: x86_64.call {imm = 256}
# CHECK-NEXT: x86_64.ret
