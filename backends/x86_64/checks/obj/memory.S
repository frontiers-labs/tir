# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

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
# CHECK-NEXT: [0x40, 0xFF, 0xE6]
# CHECK-NEXT: [0x41, 0xFF, 0xD0]
