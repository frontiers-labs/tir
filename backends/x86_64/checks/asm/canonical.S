# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

# One candidate per operation: `add eax, ebx` and `add eax, r8d` are the same
# `add32`, which spells the REX byte only for r8d. `add rcx, 42` binds the imm8
# `add_imm8s` (bits<8> tried before bits<32>); the out-of-range `add rcx, 300`
# backtracks to the imm32 `add_imm`.

	.global p
p:
	add eax, ebx
	add eax, r8d
	add rcx, 42
	add rcx, 300

# CHECK: asm.symbol {name = "p"}
# CHECK-NEXT: x86_64.add32 {dst = eax, src = ebx}
# CHECK-NEXT: x86_64.add32 {dst = eax, src = r8d}
# CHECK-NEXT: x86_64.add_imm8s {dst = rcx, imm = 42}
# CHECK-NEXT: x86_64.add_imm {dst = rcx, imm = 300}
