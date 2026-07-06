# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# ModR/M-escape memory bases: rsp/r12 need a SIB byte (rm=100), rbp/r13 with
# mod=00 need a mod=01 zero disp8 (rm=101). Reference bytes derived from GNU as
# 2.42 (`as --64`, `objdump -d -M intel`) on the equivalent forms; TIR always
# emits the REX byte (0x40 base) where `as` may omit it for 32/16/8-bit forms.
#   mov rax,[rsp]  -> 48 8b 04 24        mov rax,[rbp]  -> 48 8b 45 00
#   mov rax,[r12]  -> 49 8b 04 24        mov rax,[r13]  -> 49 8b 45 00
#   mov rax,[rsp+8]-> 48 8b 84 24 08 00 00 00

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
# disp32 with rsp base keeps the SIB (mod=10 rm=100). Divergence: TIR always
# uses disp32 for a non-zero displacement; GNU as would pick disp8 (48 8b 44 24
# 08 / 48 89 54 24 08). Both valid; TIR's is 3 bytes longer.
# CHECK-NEXT: [0x48, 0x8B, 0x84, 0x24, 0x08, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0x48, 0x89, 0x94, 0x24, 0x08, 0x00, 0x00, 0x00]
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
# mov32 load/store (REX always present).
# CHECK-NEXT: [0x40, 0x8B, 0x04, 0x24]
# CHECK-NEXT: [0x40, 0x8B, 0x45, 0x00]
# CHECK-NEXT: [0x41, 0x89, 0x14, 0x24]
# CHECK-NEXT: [0x41, 0x89, 0x55, 0x00]
# mov16 store (0x66 prefix then REX).
# CHECK-NEXT: [0x66, 0x40, 0x89, 0x14, 0x24]
# CHECK-NEXT: [0x66, 0x40, 0x89, 0x55, 0x00]
# CHECK-NEXT: [0x66, 0x41, 0x89, 0x14, 0x24]
# CHECK-NEXT: [0x66, 0x41, 0x89, 0x55, 0x00]
# mov8 store.
# CHECK-NEXT: [0x40, 0x88, 0x14, 0x24]
# CHECK-NEXT: [0x40, 0x88, 0x55, 0x00]
# CHECK-NEXT: [0x41, 0x88, 0x14, 0x24]
# CHECK-NEXT: [0x41, 0x88, 0x55, 0x00]
# A non-escape base still selects the generic form (no SIB, no disp).
# CHECK-NEXT: [0x48, 0x8B, 0x03]
