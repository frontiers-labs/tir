# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global mem_ext
mem_ext:
	movzx rax, byte ptr [rbx]
	movzx rcx, word ptr [rdx]
	movsx rax, byte ptr [rbx]
	movsx rcx, word ptr [rdx]
	movsxd rax, dword ptr [rbx]
	mov eax, [rbx]
	mov [rbx], eax
	mov [rbx], cx
	mov [rbx], dl
	mov rax, [rbx + 8]
	mov [rcx + 16], rdx
	push rbx
	push r12
	pop rbx
	pop r12

# CHECK:      asm.symbol {name = "mem_ext"}
# CHECK-NEXT: x86_64.movzx8_load {dst = rax, base = rbx}
# CHECK-NEXT: x86_64.movzx16_load {dst = rcx, base = rdx}
# CHECK-NEXT: x86_64.movsx8_load {dst = rax, base = rbx}
# CHECK-NEXT: x86_64.movsx16_load {dst = rcx, base = rdx}
# CHECK-NEXT: x86_64.movsxd_load {dst = rax, base = rbx}
# CHECK-NEXT: x86_64.mov_load32 {dst = eax, base = rbx}
# CHECK-NEXT: x86_64.mov_store32 {base = rbx, src = eax}
# CHECK-NEXT: x86_64.mov_store16 {base = rbx, src = cx}
# CHECK-NEXT: x86_64.mov_store8 {base = rbx, src = dl}
# CHECK-NEXT: x86_64.mov_load_disp {dst = rax, base = rbx, imm = 8}
# CHECK-NEXT: x86_64.mov_store_disp {base = rcx, imm = 16, src = rdx}
# CHECK-NEXT: x86_64.push_norex {reg = rbx}
# CHECK-NEXT: x86_64.push {reg = r12}
# CHECK-NEXT: x86_64.pop_norex {reg = rbx}
# CHECK-NEXT: x86_64.pop {reg = r12}
