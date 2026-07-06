# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# Canonical REX-free and imm8 (0x83) encodings, matching GNU as. Reference bytes
# from GNU as 2.42 (`as --64 -msyntax=intel -mnaked-reg`, `objdump -d -M intel`):
#   add ecx,edx  -> 01 d1          add ecx,r8d -> 44 01 c1
#   add al,bl    -> 00 d8          add sil,dil -> 40 00 fe   (empty REX required)
#   add rcx,42   -> 48 83 c1 2a    add ecx,42  -> 83 c1 2a
#   add ecx,300  -> 81 c1 2c 01 00 00
#   push rbx     -> 53             push r12    -> 41 54
#   jmp rsi      -> ff e6          sete al     -> 0f 94 c0
#   cmp ecx,edx  -> 39 d1          test al,bl  -> 84 d8
#   neg ecx      -> f7 d9          not dl      -> f6 d2
#   shl ecx,cl   -> d3 e1          rol ecx,3   -> c1 c1 03
#   neg r9d      -> 41 f7 d9       cmp r9d,r10d-> 45 39 d1   (REX still engages)
#   addsd xmm0,xmm1 -> f2 0f 58 c1 divsd xmm14,xmm15 -> f2 45 0f 5e f7

	.global canon
canon:
	add ecx, edx
	add ecx, r8d
	add al, bl
	add sil, dil
	add rcx, 42
	add ecx, 42
	add ecx, 300
	push rbx
	push r12
	jmp *rsi
	sete al
	cmp ecx, edx
	test al, bl
	neg ecx
	not dl
	shl ecx, cl
	rol ecx, 3
	neg r9d
	cmp r9d, r10d
	shl r9d, cl
	rol r9d, 3
	addsd xmm0, xmm1
	divsd xmm14, xmm15

# CHECK: canon:
# No REX for eax..edi; REX.R appears only once r8d sets its 4th bit.
# CHECK-NEXT: [0x01, 0xD1]
# CHECK-NEXT: [0x44, 0x01, 0xC1]
# al/bl need no REX; spl/bpl/sil/dil require an empty 0x40.
# CHECK-NEXT: [0x00, 0xD8]
# CHECK-NEXT: [0x40, 0x00, 0xFE]
# imm8 0x83 short form; REX.W stays for the 64-bit form.
# CHECK-NEXT: [0x48, 0x83, 0xC1, 0x2A]
# CHECK-NEXT: [0x83, 0xC1, 0x2A]
# 300 exceeds i8, so the imm32 0x81 form (still REX-free) is used.
# CHECK-NEXT: [0x81, 0xC1, 0x2C, 0x01, 0x00, 0x00]
# CHECK-NEXT: [0x53]
# CHECK-NEXT: [0x41, 0x54]
# CHECK-NEXT: [0xFF, 0xE6]
# CHECK-NEXT: [0x0F, 0x94, 0xC0]
# cmp/test reg-reg, neg/not, by-cl and rotate shifts, and low-xmm SSE drop the
# empty REX for a low register; a high register (r8+/xmm8+) keeps it.
# CHECK-NEXT: [0x39, 0xD1]
# CHECK-NEXT: [0x84, 0xD8]
# CHECK-NEXT: [0xF7, 0xD9]
# CHECK-NEXT: [0xF6, 0xD2]
# CHECK-NEXT: [0xD3, 0xE1]
# CHECK-NEXT: [0xC1, 0xC1, 0x03]
# CHECK-NEXT: [0x41, 0xF7, 0xD9]
# CHECK-NEXT: [0x45, 0x39, 0xD1]
# CHECK-NEXT: [0x41, 0xD3, 0xE1]
# CHECK-NEXT: [0x41, 0xC1, 0xC1, 0x03]
# CHECK-NEXT: [0xF2, 0x0F, 0x58, 0xC1]
# CHECK-NEXT: [0xF2, 0x45, 0x0F, 0x5E, 0xF7]
