// RUN: tir mc --march=i386 --filetype=obj-ascii %s | filecheck %s

// 32-bit AT&T parsing: the `l` mnemonics and 32-bit register names, no REX
// prefix, and the `0xB8+r` move-immediate form.

.text
.global f
f:
	movl %ecx, %eax
	addl %edx, %eax
	movl $7, %eax
	ret

// CHECK: f:
// CHECK: [0x89, 0xC8]
// CHECK: [0x01, 0xD0]
// CHECK: [0xB8, 0x07, 0x00, 0x00, 0x00]
// CHECK: [0xC3]
