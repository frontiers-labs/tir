// RUN: tir mc --march=x86-64 --filetype=obj-ascii %s | filecheck %s

// AT&T assembly parsing: `%` register sigil, `$` immediate sigil, `disp(%base)`
// memory operands, and a symbolic call. Round-trips to the same bytes the
// compiler emits.

.text
.global f
f:
	movq %rcx, %rax
	addq %rdx, %rax
	movq 16(%rbp), %rax
	movq %rax, 8(%rbp)
	movq $42, %rax
	call foo
	ret

// CHECK: f:
// CHECK: [0x48, 0x89, 0xC8]
// CHECK: [0x48, 0x01, 0xD0]
// CHECK: [0x48, 0x8B, 0x85, 0x10, 0x00, 0x00, 0x00]
// CHECK: [0x48, 0x89, 0x85, 0x08, 0x00, 0x00, 0x00]
// CHECK: [0x48, 0xC7, 0xC0, 0x2A, 0x00, 0x00, 0x00]
// CHECK: [0xE8, 0x00, 0x00, 0x00, 0x00]
// CHECK: [0xC3]
