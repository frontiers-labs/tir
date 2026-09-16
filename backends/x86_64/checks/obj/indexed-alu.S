# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

	.global indexed_alu
indexed_alu:
	add word ptr [rbx + 2*rcx], ax
	add word ptr [r10 + 2*r11], r9w
	sub word ptr [rbx + 2*rcx], ax
	sub word ptr [r10 + 2*r11], r9w
	xor ax, word ptr [rbx + 2*rcx]
	xor r9w, word ptr [r10 + 2*r11]
	inc dword ptr [rsp + 4*rax + 0x40]

# CHECK: indexed_alu:
# CHECK-NEXT: [0x66, 0x01, 0x04, 0x4B]
# CHECK-NEXT: [0x66, 0x47, 0x01, 0x0C, 0x5A]
# CHECK-NEXT: [0x66, 0x29, 0x04, 0x4B]
# CHECK-NEXT: [0x66, 0x47, 0x29, 0x0C, 0x5A]
# CHECK-NEXT: [0x66, 0x33, 0x04, 0x4B]
# CHECK-NEXT: [0x66, 0x47, 0x33, 0x0C, 0x5A]
# CHECK-NEXT: [0xFF, 0x44, 0x84, 0x40]
