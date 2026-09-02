# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# Where one encoding shape gives way to the next. Each pair below straddles a
# boundary: the last value the short form spells and the first it does not.
# Reference bytes from GNU as (`as --64 -msyntax=intel -mnaked-reg`,
# `objdump -d -M intel`), which .audit/gnu-as-check.sh re-checks: every line in
# this file assembles to the same bytes GNU as writes.

	.global bounds
bounds:
	add rcx, 127
	add rcx, 128
	add rcx, -128
	add rcx, -129
	add ecx, 127
	add ecx, -129
	add cx, 127
	add cx, -129
	mov rax, [rbx + 127]
	mov rax, [rbx + 128]
	mov rax, [rbx - 128]
	mov rax, [rbx - 129]
	mov rax, [rbx + 0]
	mov rax, [rbp + 0]
	mov rax, [rsp + 0]
	mov rax, [r13 + 0]
	mov rax, [r12 + 0]
	lea rdx, [rax + rcx]
	lea rdx, [rbp + rcx]
	lea rdx, [r13 + 4*r9]
	lea rdx, [rsp + 8*rcx]
	lea rdx, [rbx + 127]
	lea rdx, [rbx + 128]

# The 0x83 group spells a byte and sign-extends it; 0x81 spells the whole
# double word. 16-bit forms carry the operand-size prefix either way.
# CHECK: bounds:
# CHECK-NEXT: [0x48, 0x83, 0xC1, 0x7F]
# CHECK-NEXT: [0x48, 0x81, 0xC1, 0x80, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0x48, 0x83, 0xC1, 0x80]
# CHECK-NEXT: [0x48, 0x81, 0xC1, 0x7F, 0xFF, 0xFF, 0xFF]
# CHECK-NEXT: [0x83, 0xC1, 0x7F]
# CHECK-NEXT: [0x81, 0xC1, 0x7F, 0xFF, 0xFF, 0xFF]
# CHECK-NEXT: [0x66, 0x83, 0xC1, 0x7F]
# CHECK-NEXT: [0x66, 0x81, 0xC1, 0x7F, 0xFF]
# A displacement that fits a byte is spelled in one (mod=01); past that it takes
# four (mod=10). A zero displacement is left out, except through rbp and r13,
# where mod=00 would mean rip-relative, and rsp and r12 keep their SIB byte.
# CHECK-NEXT: [0x48, 0x8B, 0x43, 0x7F]
# CHECK-NEXT: [0x48, 0x8B, 0x83, 0x80, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0x48, 0x8B, 0x43, 0x80]
# CHECK-NEXT: [0x48, 0x8B, 0x83, 0x7F, 0xFF, 0xFF, 0xFF]
# CHECK-NEXT: [0x48, 0x8B, 0x03]
# CHECK-NEXT: [0x48, 0x8B, 0x45, 0x00]
# CHECK-NEXT: [0x48, 0x8B, 0x04, 0x24]
# CHECK-NEXT: [0x49, 0x8B, 0x45, 0x00]
# CHECK-NEXT: [0x49, 0x8B, 0x04, 0x24]
# An address with an index always spells the SIB byte, and rbp and r13 still
# force the displacement.
# CHECK-NEXT: [0x48, 0x8D, 0x14, 0x08]
# CHECK-NEXT: [0x48, 0x8D, 0x54, 0x0D, 0x00]
# CHECK-NEXT: [0x4B, 0x8D, 0x54, 0x8D, 0x00]
# CHECK-NEXT: [0x48, 0x8D, 0x14, 0xCC]
# CHECK-NEXT: [0x48, 0x8D, 0x53, 0x7F]
# CHECK-NEXT: [0x48, 0x8D, 0x93, 0x80, 0x00, 0x00, 0x00]
