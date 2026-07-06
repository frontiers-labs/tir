# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# Register-indirect mov and indirect jmp/call. Reference bytes from GNU as 2.42
# (`as --64 -msyntax=intel -mnaked-reg`, `objdump -d -M intel`); TIR matches:
#   mov rax,[rbx] -> 48 8b 03      mov [rcx],rdx -> 48 89 11
#   mov r9,[r10]  -> 4d 8b 0a      mov [r11],r12 -> 4d 89 23
#   jmp rsi       -> ff e6         call r8       -> 41 ff d0

	.global mem
mem:
	mov rax, [rbx]
	mov [rcx], rdx
	mov r9, [r10]
	mov [r11], r12
	jmp *rsi
	call *r8

# CHECK: mem:
# CHECK-NEXT: [0x48, 0x8B, 0x03]
# CHECK-NEXT: [0x48, 0x89, 0x11]
# CHECK-NEXT: [0x4D, 0x8B, 0x0A]
# CHECK-NEXT: [0x4D, 0x89, 0x23]
# CHECK-NEXT: [0xFF, 0xE6]
# CHECK-NEXT: [0x41, 0xFF, 0xD0]
