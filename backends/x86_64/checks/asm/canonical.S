# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

# One candidate per operation: `add eax, ebx` and `add eax, r8d` are the same
# `add32`, which spells the REX byte only for r8d, and `add rcx, 42` and
# `add rcx, 300` are the same `add_imm`, whose immediate is 32 bits wide either
# way. Whether 42 is written in one byte is a shape, decided at encode time, so
# selection has nothing left to choose between.

	.global p
p:
	add eax, ebx
	add eax, r8d
	add rcx, 42
	add rcx, 300

# CHECK: asm.symbol {name = "p"}
# CHECK-NEXT: x86_64.add32 {dst = eax, src = ebx}
# CHECK-NEXT: x86_64.add32 {dst = eax, src = r8d}
# CHECK-NEXT: x86_64.add_imm {dst = rcx, imm = 42}
# CHECK-NEXT: x86_64.add_imm {dst = rcx, imm = 300}
