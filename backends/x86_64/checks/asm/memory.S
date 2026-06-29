# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global mem
mem:
	mov rax, [rbx]
	mov [rcx], rdx
	mov r9, [r10]
	mov [r11], r12
	jmp *rsi
	call *r8

# CHECK:      asm.symbol {name = "mem"}
# CHECK-NEXT: x86_64.mov_load {dst = rax, base = rbx}
# CHECK-NEXT: x86_64.mov_store {base = rcx, src = rdx}
# CHECK-NEXT: x86_64.mov_load {dst = r9, base = r10}
# CHECK-NEXT: x86_64.mov_store {base = r11, src = r12}
# CHECK-NEXT: x86_64.jmp_indirect {target = rsi}
# CHECK-NEXT: x86_64.call_indirect {target = r8}
