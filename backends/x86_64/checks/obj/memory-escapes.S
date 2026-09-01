# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# ModR/M-escape memory bases: rsp/r12 need a SIB byte (rm=100), rbp/r13 with
# mod=00 need a mod=01 zero disp8 (rm=101). Every byte below is the byte GNU as
# 2.42 emits for the same line (`as --64 -msyntax=intel -mnaked-reg`, `objdump
# -d -M intel`), which .audit/gnu-as-check.sh re-checks.
#   mov rax,[rsp]  -> 48 8b 04 24        mov rax,[rbp]  -> 48 8b 45 00
#   mov rax,[r12]  -> 49 8b 04 24        mov rax,[r13]  -> 49 8b 45 00
#   mov rax,[rsp+8]-> 48 8b 44 24 08     mov eax,[rsp]  -> 8b 04 24

	.global esc
esc:
	mov rax, [rsp]
	mov rax, [rbp]
	mov rax, [r12]
	mov rax, [r13]
	mov [rsp], rdx
	mov [rbp], rdx
	mov [r12], rdx
	mov [r13], rdx
	mov rax, [rsp + 8]
	mov [rsp + 8], rdx
	movzx rax, byte ptr [rsp]
	movzx rax, byte ptr [rbp]
	movzx rax, word ptr [r12]
	movzx rax, word ptr [r13]
	movsx rax, byte ptr [rsp]
	movsx rax, word ptr [r13]
	movsxd rax, dword ptr [rsp]
	movsxd rax, dword ptr [rbp]
	movsxd rax, dword ptr [r12]
	movsxd rax, dword ptr [r13]
	mov eax, [rsp]
	mov eax, [rbp]
	mov [r12], edx
	mov [r13], edx
	mov [rsp], dx
	mov [rbp], dx
	mov [r12], dx
	mov [r13], dx
	mov [rsp], dl
	mov [rbp], dl
	mov [r12], dl
	mov [r13], dl
	mov rax, [rbx]

	mov rax, [rsp + 8]
	mov [rbp + 8], rdx
	mov ecx, [r12 - 200]
	mov [r13 + 1024], r9d
	mov [rsp + 8], dil
	mov [rbp + 0], sil

# CHECK: esc:
# 64-bit mov load/store: SIB 0x24 for rsp/r12, mod=01 disp8=0 for rbp/r13.
# CHECK-NEXT: [0x48, 0x8B, 0x04, 0x24]
# CHECK-NEXT: [0x48, 0x8B, 0x45, 0x00]
# CHECK-NEXT: [0x49, 0x8B, 0x04, 0x24]
# CHECK-NEXT: [0x49, 0x8B, 0x45, 0x00]
# CHECK-NEXT: [0x48, 0x89, 0x14, 0x24]
# CHECK-NEXT: [0x48, 0x89, 0x55, 0x00]
# CHECK-NEXT: [0x49, 0x89, 0x14, 0x24]
# CHECK-NEXT: [0x49, 0x89, 0x55, 0x00]
# A displacement that fits a byte is spelled in one (mod=01), with the SIB the
# rsp base needs.
# CHECK-NEXT: [0x48, 0x8B, 0x44, 0x24, 0x08]
# CHECK-NEXT: [0x48, 0x89, 0x54, 0x24, 0x08]
# movzx/movsx byte/word.
# CHECK-NEXT: [0x48, 0x0F, 0xB6, 0x04, 0x24]
# CHECK-NEXT: [0x48, 0x0F, 0xB6, 0x45, 0x00]
# CHECK-NEXT: [0x49, 0x0F, 0xB7, 0x04, 0x24]
# CHECK-NEXT: [0x49, 0x0F, 0xB7, 0x45, 0x00]
# CHECK-NEXT: [0x48, 0x0F, 0xBE, 0x04, 0x24]
# CHECK-NEXT: [0x49, 0x0F, 0xBF, 0x45, 0x00]
# movsxd.
# CHECK-NEXT: [0x48, 0x63, 0x04, 0x24]
# CHECK-NEXT: [0x48, 0x63, 0x45, 0x00]
# CHECK-NEXT: [0x49, 0x63, 0x04, 0x24]
# CHECK-NEXT: [0x49, 0x63, 0x45, 0x00]
# mov32 load/store: no REX byte, none of these registers needs one.
# CHECK-NEXT: [0x8B, 0x04, 0x24]
# CHECK-NEXT: [0x8B, 0x45, 0x00]
# CHECK-NEXT: [0x41, 0x89, 0x14, 0x24]
# CHECK-NEXT: [0x41, 0x89, 0x55, 0x00]
# mov16 store: the operand-size prefix, and no REX.
# CHECK-NEXT: [0x66, 0x89, 0x14, 0x24]
# CHECK-NEXT: [0x66, 0x89, 0x55, 0x00]
# CHECK-NEXT: [0x66, 0x41, 0x89, 0x14, 0x24]
# CHECK-NEXT: [0x66, 0x41, 0x89, 0x55, 0x00]
# mov8 store.
# CHECK-NEXT: [0x88, 0x14, 0x24]
# CHECK-NEXT: [0x88, 0x55, 0x00]
# CHECK-NEXT: [0x41, 0x88, 0x14, 0x24]
# CHECK-NEXT: [0x41, 0x88, 0x55, 0x00]
# A non-escape base still selects the generic form (no SIB, no disp).
# CHECK-NEXT: [0x48, 0x8B, 0x03]
# A displacement decides how many bytes of it the instruction spells, and the
# escape bases keep their SIB byte through both. An 8-bit register operand
# carries the REX byte that names it, whatever the address looks like.
# CHECK-NEXT: [0x48, 0x8B, 0x44, 0x24, 0x08]
# CHECK-NEXT: [0x48, 0x89, 0x55, 0x08]
# CHECK-NEXT: [0x41, 0x8B, 0x8C, 0x24, 0x38, 0xFF, 0xFF, 0xFF]
# CHECK-NEXT: [0x45, 0x89, 0x8D, 0x00, 0x04, 0x00, 0x00]
# CHECK-NEXT: [0x40, 0x88, 0x7C, 0x24, 0x08]
# CHECK-NEXT: [0x40, 0x88, 0x75, 0x00]
