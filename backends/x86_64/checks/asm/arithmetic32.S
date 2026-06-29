# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s

	.global alu32
alu32:
	add eax, ebx
	sub ecx, edx
	and esi, edi
	or ebp, esp
	xor eax, eax
	mov ebx, ecx
	add eax, 42
	and ecx, 255
	or edx, 1
	xor esi, 7
	mov edi, 100
	shl eax, 3
	shr ebx, 1
	sar ecx, 31
	add r8d, r9d
	mov r15d, r10d
	add r12d, 42

# CHECK:      asm.symbol {name = "alu32"}
# CHECK-NEXT: x86_64.add32 {dst = eax, src = ebx}
# CHECK-NEXT: x86_64.sub32 {dst = ecx, src = edx}
# CHECK-NEXT: x86_64.and32 {dst = esi, src = edi}
# CHECK-NEXT: x86_64.or32 {dst = ebp, src = esp}
# CHECK-NEXT: x86_64.xor32 {dst = eax, src = eax}
# CHECK-NEXT: x86_64.mov32 {dst = ebx, src = ecx}
# CHECK-NEXT: x86_64.add_imm32 {dst = eax, imm = 42}
# CHECK-NEXT: x86_64.and_imm32 {dst = ecx, imm = 255}
# CHECK-NEXT: x86_64.or_imm32 {dst = edx, imm = 1}
# CHECK-NEXT: x86_64.xor_imm32 {dst = esi, imm = 7}
# CHECK-NEXT: x86_64.mov_imm32 {dst = edi, imm = 100}
# CHECK-NEXT: x86_64.shl32 {dst = eax, imm = 3}
# CHECK-NEXT: x86_64.shr32 {dst = ebx, imm = 1}
# CHECK-NEXT: x86_64.sar32 {dst = ecx, imm = 31}
# CHECK-NEXT: x86_64.add32 {dst = r8d, src = r9d}
# CHECK-NEXT: x86_64.mov32 {dst = r15d, src = r10d}
# CHECK-NEXT: x86_64.add_imm32 {dst = r12d, imm = 42}
