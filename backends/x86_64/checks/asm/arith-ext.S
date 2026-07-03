# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global arith_ext
arith_ext:
	neg rax
	not rcx
	sub rax, 42
	cmp rbx, 7
	test rdx, 5
	imul rbx
	shl rax, cl
	shr r8, cl
	sar rdx, cl
	rol rax, 3
	ror rbx, 2
	movzx rax, bl
	movzx rcx, dx
	movsx rax, bl
	movsx rcx, dx
	movsxd rax, ebx
	movabs rax, 81985529216486895
	movabs r9, 2

# CHECK:      asm.symbol {name = "arith_ext"}
# CHECK-NEXT: x86_64.neg {dst = rax}
# CHECK-NEXT: x86_64.not {dst = rcx}
# CHECK-NEXT: x86_64.sub_imm {dst = rax, imm = 42}
# CHECK-NEXT: x86_64.cmp_imm {dst = rbx, imm = 7}
# CHECK-NEXT: x86_64.test_imm {dst = rdx, imm = 5}
# CHECK-NEXT: x86_64.imul_wide {dst = rbx}
# CHECK-NEXT: x86_64.shl_cl {dst = rax}
# CHECK-NEXT: x86_64.shr_cl {dst = r8}
# CHECK-NEXT: x86_64.sar_cl {dst = rdx}
# CHECK-NEXT: x86_64.rol {dst = rax, imm = 3}
# CHECK-NEXT: x86_64.ror {dst = rbx, imm = 2}
# CHECK-NEXT: x86_64.movzx8 {dst = rax, src = bl}
# CHECK-NEXT: x86_64.movzx16 {dst = rcx, src = dx}
# CHECK-NEXT: x86_64.movsx8 {dst = rax, src = bl}
# CHECK-NEXT: x86_64.movsx16 {dst = rcx, src = dx}
# CHECK-NEXT: x86_64.movsxd {dst = rax, src = ebx}
# CHECK-NEXT: x86_64.movabs {dst = rax, imm = 81985529216486895}
# CHECK-NEXT: x86_64.movabs {dst = r9, imm = 2}
