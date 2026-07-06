# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global arith_ext32
arith_ext32:
	neg eax
	not ecx
	sub eax, 42
	cmp ebx, ecx
	cmp ebx, 7
	test edx, esi
	test edx, 5
	shl eax, cl
	shr ebx, cl
	sar ecx, cl
	rol eax, 3
	ror ebx, 2

# CHECK:      asm.symbol {name = "arith_ext32"}
# CHECK-NEXT: x86_64.neg32_norex {dst = eax}
# CHECK-NEXT: x86_64.not32_norex {dst = ecx}
# CHECK-NEXT: x86_64.sub_imm8s32_norex {dst = eax, imm = 42}
# CHECK-NEXT: x86_64.cmp32_norex {dst = ebx, src = ecx}
# CHECK-NEXT: x86_64.cmp_imm8s32_norex {dst = ebx, imm = 7}
# CHECK-NEXT: x86_64.test32_norex {dst = edx, src = esi}
# CHECK-NEXT: x86_64.test_imm32_norex {dst = edx, imm = 5}
# CHECK-NEXT: x86_64.shl_cl32_norex {dst = eax}
# CHECK-NEXT: x86_64.shr_cl32_norex {dst = ebx}
# CHECK-NEXT: x86_64.sar_cl32_norex {dst = ecx}
# CHECK-NEXT: x86_64.rol32_norex {dst = eax, imm = 3}
# CHECK-NEXT: x86_64.ror32_norex {dst = ebx, imm = 2}
