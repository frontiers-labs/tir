# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

# The assembler tries the most-constrained candidate first. `add eax, ebx` binds
# the REX-free `add32_norex` (both operands in the 8-register GPR32low class);
# `add eax, r8d` cannot (r8d is not low), so it backtracks to the REX `add32`.
# `add rcx, 42` binds the imm8 `add_imm8s` (bits<8> tried before bits<32>); the
# out-of-range `add rcx, 300` backtracks to the imm32 `add_imm`.

	.global p
p:
	add eax, ebx
	add eax, r8d
	add rcx, 42
	add rcx, 300

# CHECK: asm.symbol {name = "p"}
# CHECK-NEXT: x86_64.add32_norex {dst = eax, src = ebx}
# CHECK-NEXT: x86_64.add32 {dst = eax, src = r8d}
# CHECK-NEXT: x86_64.add_imm8s {dst = rcx, imm = 42}
# CHECK-NEXT: x86_64.add_imm {dst = rcx, imm = 300}
