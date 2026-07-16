# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global operand_widths
operand_widths:
	add al, bl
	add ax, bx
	add eax, ebx
	add rax, rbx

# CHECK:      asm.symbol {name = "operand_widths"}
# CHECK-NEXT: x86_64.add8_norex {dst = al, src = bl}
# CHECK-NEXT: x86_64.add16_norex {dst = ax, src = bx}
# CHECK-NEXT: x86_64.add32_norex {dst = eax, src = ebx}
# CHECK-NEXT: x86_64.add {dst = rax, src = rbx}
